//! Minimal Server-Sent Events parser for `GET /v1/events` (SPEC §11 #32).

/// One dispatched event: the block's `id`, `event` and joined `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: Option<u64>,
    pub event: String,
    pub data: String,
}

/// Feed it raw chunks; it returns the frames each chunk completes.
#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    id: Option<u64>,
    event: String,
    data: Vec<String>,
}

impl Parser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    frames.push(Frame {
                        id: self.id,
                        event: std::mem::take(&mut self.event),
                        data: self.data.join("\n"),
                    });
                }
                self.id = None;
                self.event.clear();
                self.data.clear();
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "id" => self.id = value.parse().ok(),
                "event" => self.event = value.to_owned(),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_chunks_and_comments_are_skipped() {
        let mut p = Parser::default();
        assert!(
            p.push(b": keep-alive\n\nid: 7\nevent: progress\nda")
                .is_empty()
        );
        let frames = p.push("ta: {\"step\":\"Reading…\"}\n\n".as_bytes());
        assert_eq!(
            frames,
            [Frame {
                id: Some(7),
                event: "progress".into(),
                data: "{\"step\":\"Reading…\"}".into(),
            }]
        );
    }

    #[test]
    fn multi_line_data_is_joined_and_a_block_without_data_is_dropped() {
        let mut p = Parser::default();
        let frames = p.push(b"id: 3\n\nevent: x\ndata: a\ndata: b\r\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "a\nb");
        assert_eq!(frames[0].id, None);
    }

    #[test]
    fn a_split_multibyte_character_survives() {
        let mut p = Parser::default();
        let bytes = "data: …\n\n".as_bytes();
        assert!(p.push(&bytes[..7]).is_empty());
        assert_eq!(p.push(&bytes[7..])[0].data, "…");
    }
}
