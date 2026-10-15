//! One Tesseract reading parsed from its TSV output: words with per-word
//! confidence, so readings can be compared and doubtful words flagged
//! instead of handed to the model as if certain (SPEC §4.4).

/// Words below this confidence add nothing to a reading's score: they are
/// mostly noise from logos, rules and paper edges.
const MIN_COUNTED_CONF: f32 = 30.0;
/// A word with fewer alphanumeric characters than this share is a fragment
/// of a printed rule or border (`———`, `—..—[—`), never content.
const MIN_ALNUM_RATIO: f32 = 0.5;
/// Digits must make up this share of a word (and be at least two) for it to
/// count as a number, which keeps an otherwise doubtful line visible.
const NUMBER_DIGIT_RATIO: f32 = 0.7;

fn ratio(text: &str, pred: impl Fn(char) -> bool) -> f32 {
    let total = text.chars().count().max(1);
    text.chars().filter(|c| pred(*c)).count() as f32 / total as f32
}

fn is_number_like(text: &str) -> bool {
    text.chars().filter(char::is_ascii_digit).count() >= 2
        && ratio(text, |c| c.is_ascii_digit()) >= NUMBER_DIGIT_RATIO
}

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    /// 0–100 as reported by Tesseract.
    pub conf: f32,
    /// `(block, paragraph, line)`; a change starts a new output line.
    pub line: (u32, u32, u32),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    pub words: Vec<Word>,
}

impl Reading {
    /// Parses `tesseract … tsv`. Only level-5 (word) rows carry text; rows
    /// that don't parse are skipped rather than failing the whole reading.
    pub fn from_tsv(tsv: &str) -> Self {
        let words = tsv
            .lines()
            .skip(1)
            .filter_map(|row| {
                let cols: Vec<&str> = row.splitn(12, '\t').collect();
                if cols.len() < 12 || cols[0] != "5" {
                    return None;
                }
                let text = cols[11].trim();
                if text.is_empty() {
                    return None;
                }
                Some(Word {
                    text: text.to_owned(),
                    conf: cols[10].parse().ok()?,
                    line: (
                        cols[2].parse().ok()?,
                        cols[3].parse().ok()?,
                        cols[4].parse().ok()?,
                    ),
                })
            })
            .collect();
        Self { words }
    }

    /// Confidence-weighted word count: rewards both covering more of the
    /// page and being sure about it.
    pub fn score(&self) -> f32 {
        self.words
            .iter()
            .filter(|w| w.conf >= MIN_COUNTED_CONF)
            .map(|w| w.conf / 100.0)
            .sum()
    }

    pub fn mean_conf(&self) -> f32 {
        if self.words.is_empty() {
            return 0.0;
        }
        self.words.iter().map(|w| w.conf).sum::<f32>() / self.words.len() as f32
    }

    pub fn doubtful(&self, below: f32) -> usize {
        self.words.iter().filter(|w| w.conf < below).count()
    }

    /// Drops what can only be noise before the text reaches the model:
    /// rule/border fragments, and lines where every word is doubtful unless
    /// one of them is a number. Returns the trimmed reading and how many
    /// lines were dropped.
    pub fn trimmed(&self, below: f32) -> (Reading, usize) {
        let mut kept = Vec::new();
        let mut dropped = 0;
        let mut start = 0;
        while start < self.words.len() {
            let line = self.words[start].line;
            let end = self.words[start..]
                .iter()
                .position(|w| w.line != line)
                .map_or(self.words.len(), |n| start + n);
            let words: Vec<&Word> = self.words[start..end]
                .iter()
                .filter(|w| ratio(&w.text, char::is_alphanumeric) >= MIN_ALNUM_RATIO)
                .collect();
            let noise = words.is_empty()
                || words
                    .iter()
                    .all(|w| w.conf < below && !is_number_like(&w.text));
            if noise {
                dropped += 1;
            } else {
                kept.extend(words.into_iter().cloned());
            }
            start = end;
        }
        (Reading { words: kept }, dropped)
    }

    /// Plain text with each word under `below` written as `[?word]`.
    pub fn to_text(&self, below: f32) -> String {
        let mut out = String::new();
        let mut prev: Option<(u32, u32, u32)> = None;
        for w in &self.words {
            match prev {
                Some(p) if p == w.line => out.push(' '),
                Some(p) if p.0 != w.line.0 => out.push_str("\n\n"),
                Some(_) => out.push('\n'),
                None => {}
            }
            if w.conf < below {
                out.push_str(&format!("[?{}]", w.text));
            } else {
                out.push_str(&w.text);
            }
            prev = Some(w.line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TSV: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
1\t1\t0\t0\t0\t0\t0\t0\t100\t100\t-1\t
5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t96.5\tTotal
5\t1\t1\t1\t1\t2\t0\t0\t10\t10\t91.0\ta
5\t1\t1\t1\t1\t3\t0\t0\t10\t10\t95.0\tPagar
5\t1\t1\t1\t1\t4\t0\t0\t10\t10\t41.2\tQ30.46
5\t1\t1\t1\t2\t1\t0\t0\t10\t10\t88.0\tIVA
5\t1\t2\t1\t1\t1\t0\t0\t10\t10\t12.0\t~
5\t1\t2\t1\t1\t2\t0\t0\t10\t10\t90.0\t
garbage row
";

    #[test]
    fn parses_word_rows_only() {
        let r = Reading::from_tsv(TSV);
        assert_eq!(r.words.len(), 6);
        assert_eq!(r.words[3].text, "Q30.46");
        assert_eq!(r.words[3].conf, 41.2);
    }

    #[test]
    fn text_keeps_lines_and_marks_doubtful_words() {
        let r = Reading::from_tsv(TSV);
        assert_eq!(r.to_text(60.0), "Total a Pagar [?Q30.46]\nIVA\n\n[?~]");
        assert_eq!(r.doubtful(60.0), 2);
    }

    fn word(text: &str, conf: f32, line: u32) -> Word {
        Word {
            text: text.into(),
            conf,
            line: (1, 1, line),
        }
    }

    #[test]
    fn trimming_drops_rule_fragments_and_all_doubtful_lines() {
        let r = Reading {
            words: vec![
                word("[?fF]", 20.0, 1),
                word("pa", 30.0, 1),
                word("11", 95.0, 2),
                word("5-35,Z0NA1,", 88.0, 2),
                word("(FEL)", 90.0, 2),
                word("———.——]—Ñ2=——", 0.0, 2),
                word("—..—[—", 10.0, 3),
                word("+", 95.0, 3),
                word("Seriio___", 20.0, 4),
                word("?$222ÑÑ%qMm", 15.0, 4),
            ],
        };
        let (t, dropped) = r.trimmed(60.0);
        assert_eq!(t.to_text(60.0), "11 5-35,Z0NA1, (FEL)");
        assert_eq!(dropped, 3);
    }

    #[test]
    fn trimming_keeps_doubtful_values_worth_seeing() {
        let r = Reading {
            words: vec![
                word("Alumbrado", 95.0, 1),
                word("Q", 40.0, 1),
                word("10", 23.0, 1),
                word("30.46", 41.0, 2),
                word("Código", 46.0, 3),
                word("654321", 46.0, 3),
            ],
        };
        let (t, dropped) = r.trimmed(60.0);
        assert_eq!(dropped, 0);
        assert_eq!(
            t.to_text(60.0),
            "Alumbrado [?Q] [?10]\n[?30.46]\n[?Código] [?654321]"
        );
    }

    #[test]
    fn score_ignores_noise_and_rewards_confident_coverage() {
        let r = Reading::from_tsv(TSV);
        let expected = 0.965 + 0.91 + 0.95 + 0.412 + 0.88;
        assert!((r.score() - expected).abs() < 1e-4, "{}", r.score());
        assert_eq!(Reading::default().score(), 0.0);
        assert_eq!(Reading::default().mean_conf(), 0.0);
    }
}
