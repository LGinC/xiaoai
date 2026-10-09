use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct Header {
    pub dialog_id: String,
    pub id: String,
    pub name: String,
    pub namespace: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct TextResult {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct Payload {
    #[serde(default)]
    pub is_final: bool,
    #[serde(default)]
    pub results: Vec<TextResult>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RecognizeResult {
    pub header: Header,
    pub payload: Payload,
}

impl RecognizeResult {
    pub fn asr_text(&self) -> Option<&str> {
        if self.header.namespace != "SpeechRecognizer" || self.header.name != "RecognizeResult" {
            return None;
        }
        let text = self.payload.results.first()?.text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RecognizeResult;

    #[test]
    fn parses_final_recognize_result() {
        let line = r#"{"header":{"dialog_id":"d1","id":"id-1","name":"RecognizeResult","namespace":"SpeechRecognizer"},"payload":{"is_final":true,"results":[{"text":"打开PC"}]}}"#;
        let parsed: RecognizeResult = serde_json::from_str(line).unwrap();
        assert!(parsed.payload.is_final);
        assert_eq!(parsed.asr_text(), Some("打开PC"));
    }

    #[test]
    fn ignores_non_asr_instructions() {
        let line = r#"{"header":{"dialog_id":"d1","id":"id-2","name":"Speak","namespace":"SpeechSynthesizer"},"payload":{"is_final":false,"results":[]}}"#;
        let parsed: RecognizeResult = serde_json::from_str(line).unwrap();
        assert_eq!(parsed.asr_text(), None);
    }
}
