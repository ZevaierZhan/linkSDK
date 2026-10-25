//! Bounded framing shared by independent libzt services.
use serde::Serialize;
pub const MAX_FRAME: usize = 65536;
#[derive(Clone)]
pub struct Frame {
    pub kind: u8,
    pub data: Vec<u8>,
}
impl Frame {
    pub fn json<T: Serialize>(kind: u8, data: &T) -> Result<Self, String> {
        Ok(Self {
            kind,
            data: serde_json::to_vec(data).map_err(|e| e.to_string())?,
        })
    }
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_slice(&self.data).map_err(|e| e.to_string())
    }
    pub fn encode(self) -> Result<Vec<u8>, String> {
        if self.data.len() > MAX_FRAME {
            return Err("frame too large".into());
        }
        let mut b = vec![self.kind];
        b.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        b.extend(self.data);
        Ok(b)
    }
}
