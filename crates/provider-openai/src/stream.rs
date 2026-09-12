use crate::TransportError;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_FRAME: usize = 1_048_576;
#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

/// Only complete SSE responses become a provider response. Draft text is the
/// sole incremental output; fragmented tool arguments are never exposed.
#[derive(Default)]
pub(crate) struct Decoder {
    pending: Vec<u8>,
    data: String,
    content: String,
    calls: BTreeMap<usize, Call>,
    finish_reason: Option<String>,
    usage: Value,
    done: bool,
}
fn error(message: impl Into<String>) -> TransportError {
    TransportError::Protocol(message.into())
}

impl Decoder {
    pub fn push(
        &mut self,
        bytes: &[u8],
        text: &mut dyn FnMut(String),
    ) -> Result<(), TransportError> {
        for byte in bytes {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.pending);
                let line = std::str::from_utf8(&line)
                    .map_err(|_| error("stream contains invalid UTF-8"))?
                    .trim_end_matches('\r');
                self.line(line, text)?;
            } else {
                self.pending.push(*byte);
                if self.pending.len() > MAX_FRAME {
                    return Err(error("stream line exceeds 1 MiB"));
                }
            }
        }
        Ok(())
    }
    fn line(&mut self, line: &str, text: &mut dyn FnMut(String)) -> Result<(), TransportError> {
        if line.is_empty() {
            return self.dispatch(text);
        }
        if let Some(data) = line.strip_prefix("data:") {
            if self.done {
                return Err(error("stream data followed [DONE]"));
            }
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            self.data.push('\n');
            if self.data.len() > MAX_FRAME {
                return Err(error("stream event exceeds 1 MiB"));
            }
        }
        // SSE comments, event/id fields and curl's terminal timing line carry no
        // model data. Non-SSE HTTP error bodies are handled by the transport.
        Ok(())
    }
    fn dispatch(&mut self, text: &mut dyn FnMut(String)) -> Result<(), TransportError> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data);
        if data.trim() == "[DONE]" {
            if self.finish_reason.is_none() {
                return Err(error("stream ended without a finish reason"));
            }
            self.done = true;
            return Ok(());
        }
        let frame: Value =
            serde_json::from_str(&data).map_err(|e| error(format!("invalid stream JSON: {e}")))?;
        if frame.get("error").is_some() {
            return Err(error(format!("provider stream error: {}", frame["error"])));
        }
        if let Some(usage) = frame.get("usage").filter(|v| !v.is_null()) {
            self.usage = usage.clone();
        }
        let choices = frame
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| error("stream frame has no choices"))?;
        if choices.len() > 1 {
            return Err(error("multiple streaming choices are unsupported"));
        }
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64) != Some(0) {
                return Err(error("unexpected stream choice index"));
            }
            if self.finish_reason.is_some() {
                return Err(error("choice data followed its finish reason"));
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").filter(|v| !v.is_null()) {
                    let content = content
                        .as_str()
                        .ok_or_else(|| error("stream text delta must be a string"))?;
                    if !content.is_empty() {
                        self.content.push_str(content);
                        text(content.to_owned());
                    }
                }
                if delta.get("function_call").is_some() {
                    return Err(error("legacy streamed function_call is unsupported"));
                }
                if let Some(calls) = delta.get("tool_calls") {
                    for call in calls
                        .as_array()
                        .ok_or_else(|| error("stream tool_calls must be an array"))?
                    {
                        let index = call
                            .get("index")
                            .and_then(Value::as_u64)
                            .filter(|n| *n < 128)
                            .ok_or_else(|| error("stream tool index must be below 128"))?
                            as usize;
                        let collected = self.calls.entry(index).or_default();
                        if let Some(id) = call.get("id").filter(|v| !v.is_null()) {
                            collected.id.push_str(
                                id.as_str().ok_or_else(|| error("invalid stream call id"))?,
                            );
                        }
                        if let Some(kind) = call.get("type").filter(|v| !v.is_null())
                            && kind != "function"
                        {
                            return Err(error("unsupported streamed tool type"));
                        }
                        if let Some(function) = call.get("function") {
                            if let Some(name) = function.get("name").filter(|v| !v.is_null()) {
                                collected.name.push_str(
                                    name.as_str()
                                        .ok_or_else(|| error("invalid streamed function name"))?,
                                );
                            }
                            if let Some(arguments) =
                                function.get("arguments").filter(|v| !v.is_null())
                            {
                                collected.arguments.push_str(
                                    arguments.as_str().ok_or_else(|| {
                                        error("invalid streamed arguments fragment")
                                    })?,
                                );
                            }
                        }
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
                self.finish_reason = Some(
                    reason
                        .as_str()
                        .ok_or_else(|| error("invalid stream finish reason"))?
                        .to_owned(),
                );
            }
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<Value, TransportError> {
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            let line =
                std::str::from_utf8(&pending).map_err(|_| error("truncated UTF-8 stream"))?;
            self.line(line.trim_end_matches('\r'), &mut |_| {})?;
        }
        self.dispatch(&mut |_| {})?;
        if !self.done {
            return Err(error("incomplete stream: missing [DONE]"));
        }
        let mut calls = Vec::new();
        for (expected, (index, call)) in self.calls.into_iter().enumerate() {
            if index != expected
                || call.id.is_empty()
                || call.name.is_empty()
                || call.arguments.is_empty()
            {
                return Err(error("incomplete streamed tool call"));
            }
            calls.push(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}));
        }
        Ok(
            json!({"choices":[{"message":{"content":self.content,"tool_calls":calls},"finish_reason":self.finish_reason}],"usage":self.usage}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_split_unicode_and_interleaved_tool_arguments_assemble_only_at_completion() {
        let frames = [
            json!({"choices":[{"index":0,"delta":{"content":"猫","tool_calls":[{"index":0,"id":"a","function":{"name":"write","arguments":"{\"x\":"}},{"index":1,"id":"b","function":{"name":"read","arguments":"{"}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"}"}},{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":7}}),
        ];
        let stream = format!(
            ": keepalive\r\n\r\n{}data: [DONE]\r\n\r\n",
            frames
                .iter()
                .map(|v| format!("data: {v}\r\n\r\n"))
                .collect::<String>()
        );
        let mut decoder = Decoder::default();
        let mut draft = String::new();
        for byte in stream.as_bytes() {
            decoder.push(&[*byte], &mut |s| draft.push_str(&s)).unwrap();
        }
        assert_eq!(draft, "猫");
        let response = crate::parse_response(decoder.finish().unwrap()).unwrap();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].arguments, json!({"x":1}));
        assert_eq!(response.tool_calls[1].arguments, json!({}));
        assert_eq!(response.usage.output_tokens, 7);
    }
    #[test]
    fn incomplete_or_conflicting_streams_never_become_a_model_response() {
        for stream in [
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"error\":{\"message\":\"failed\"}}\n\n",
            "data: {broken}\n\n",
            "data: {\"choices\":[{\"index\":1,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\ndata: {}\n\n",
        ] {
            let mut decoder = Decoder::default();
            assert!(
                decoder
                    .push(stream.as_bytes(), &mut |_| {})
                    .and_then(|_| decoder.finish())
                    .is_err(),
                "{stream}"
            );
        }
    }
}
