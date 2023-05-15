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
    pub text: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct Payload {
    pub is_final: bool,
    pub results: Vec<TextResult>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RecognizeResult {
    pub header: Header,
    pub payload: Payload,
}
