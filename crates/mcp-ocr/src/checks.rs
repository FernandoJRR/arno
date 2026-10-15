//! Misread checks (SPEC §11 #20): flag a number that fits nothing on the page
//! but would, if one commonly confused digit were different. Pure arithmetic
//! on the recognized text — no document types, no field names. Faded print
//! (an "8" that prints like a "3") reads at high confidence, so this is the
//! only signal that catches it.

use std::collections::BTreeSet;

/// Values are compared in units of 1e-5, enough for tariffs like 0.84321.
const SCALE: f64 = 100_000.0;
/// A number counts as explained only by close neighbours, so a coincidence
/// further away (5104 + 10 = 5114 with an unrelated fee) can't mask a misread.
const EXPLAINED_WINDOW: usize = 3;
/// A repair may draw on partners this far away.
const REPAIR_WINDOW: usize = 6;
/// Fewer significant digits than this and coincidences dominate.
const MIN_SIGNIFICANT_DIGITS: usize = 3;
/// Longer integers are IDs, not amounts.
const MAX_INTEGER_DIGITS: usize = 6;
const MAX_CHECKS: usize = 5;
/// Digit pairs OCR and faded print commonly swap.
const CONFUSABLE: &[(char, char)] = &[
    ('3', '8'),
    ('0', '8'),
    ('0', '9'),
    ('0', '6'),
    ('1', '7'),
    ('5', '6'),
    ('6', '8'),
    ('5', '9'),
];
/// Words naming a hundredth of a currency unit; with an amount in words
/// they turn "80 … 46 centavos" into 80.46.
const SUBUNIT_WORDS: &[&str] = &["centavo", "centimo", "céntimo", "cent"];

#[derive(Debug, Clone)]
struct Num {
    raw: String,
    /// The token exactly as in the text (`30 46` when merged).
    span: String,
    values: Vec<i64>,
    line: usize,
    /// An integer glued to a letter (`C131`, `R147`): a reference code, so
    /// never repaired, though it may still be a partner (`Q10`).
    code: bool,
}

/// A likely misread. `strong` repairs (two independent pieces of evidence)
/// are applied to the text; the rest are only reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Misread {
    pub line: usize,
    /// Exactly as it appears in the text (`30 46` for a dropped decimal point).
    pub span: String,
    pub raw: String,
    pub fixed: String,
    pub why: Vec<String>,
    pub strong: bool,
}

impl Misread {
    pub fn message(&self) -> String {
        let verb = if self.strong {
            "was corrected to"
        } else {
            "may be a misread of"
        };
        format!(
            "\"{}\" {verb} {} ({})",
            self.raw,
            self.fixed,
            self.why.join("; ")
        )
    }
}

/// One reason a value fits: an arithmetic relation with the numbers at
/// `partners`, or a match with an amount written in words.
#[derive(Debug, Clone)]
struct Evidence {
    text: String,
    partners: Vec<usize>,
    words: bool,
}

/// Every likely misread, one per occurrence; at most `MAX_CHECKS` distinct
/// `(raw, fixed)` pairs.
pub fn find_misreads(text: &str) -> Vec<Misread> {
    let lines: Vec<&str> = text.lines().collect();
    let nums: Vec<Num> = lines
        .iter()
        .enumerate()
        .flat_map(|(i, l)| extract(l, i))
        .collect();
    let words: Vec<(usize, i64)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| amount_in_words(l).map(|v| (i, v)))
        .collect();

    let mut out: Vec<Misread> = Vec::new();
    let mut distinct = BTreeSet::new();
    for (i, n) in nums.iter().enumerate() {
        if n.code
            || significant_digits(&n.raw) < MIN_SIGNIFICANT_DIGITS
            || n.values
                .iter()
                .any(|&v| !relations(v, i, n.line, EXPLAINED_WINDOW, &nums, &words).is_empty())
        {
            continue;
        }
        let mut repairs: Vec<(String, Vec<Evidence>)> = variants(&n.raw)
            .into_iter()
            .filter_map(|raw| {
                let ev: Vec<Evidence> = parse(&raw)
                    .into_iter()
                    .flat_map(|v| relations(v, i, n.line, REPAIR_WINDOW, &nums, &words))
                    .collect();
                (!ev.is_empty()).then_some((raw, ev))
            })
            .collect();
        repairs.dedup_by(|a, b| a.0 == b.0);
        // Two different repairs that both fit is a coincidence, not evidence.
        let [(fixed, ev)] = repairs.as_slice() else {
            continue;
        };
        let key = (n.raw.clone(), fixed.clone());
        if !distinct.contains(&key) && distinct.len() >= MAX_CHECKS {
            continue;
        }
        distinct.insert(key);
        let mut why: Vec<String> = ev.iter().map(|e| e.text.clone()).collect();
        why.dedup();
        out.push(Misread {
            line: n.line,
            span: n.span.clone(),
            raw: n.raw.clone(),
            fixed: fixed.clone(),
            why,
            strong: is_strong(ev),
        });
    }
    out
}

/// The text with strong misreads corrected, the user-facing check messages,
/// and how many corrections were applied.
pub fn correct(text: &str) -> (String, Vec<String>, usize) {
    let mut found = find_misreads(text);
    let corrected = apply_corrections(text, &mut found);
    let applied = found.iter().filter(|m| m.strong).count();
    (corrected, messages(&found), applied)
}

/// Two independent pieces of evidence: the amount in words plus a relation,
/// or two relations sharing no partner.
fn is_strong(ev: &[Evidence]) -> bool {
    let arith: Vec<&Evidence> = ev.iter().filter(|e| !e.words).collect();
    if ev.iter().any(|e| e.words) && !arith.is_empty() {
        return true;
    }
    arith.iter().enumerate().any(|(k, a)| {
        arith[k + 1..]
            .iter()
            .any(|b| a.partners.iter().all(|p| !b.partners.contains(p)))
    })
}

/// Rewrites each strong misread in place as `fixed [corrected from span]`.
/// A span not found as a whole token on its line is left alone and the
/// misread downgraded to a report.
pub fn apply_corrections(text: &str, misreads: &mut [Misread]) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for m in misreads.iter_mut().filter(|m| m.strong) {
        let Some(line) = lines.get_mut(m.line) else {
            m.strong = false;
            continue;
        };
        match find_token(line, &m.span) {
            Some(at) => line.replace_range(
                at..at + m.span.len(),
                &format!("{} [corrected from {}]", m.fixed, m.span),
            ),
            None => m.strong = false,
        }
    }
    lines.join("\n")
}

/// One message per distinct `(raw, fixed)`, strong if any occurrence was.
pub fn messages(misreads: &[Misread]) -> Vec<String> {
    let mut out: Vec<Misread> = Vec::new();
    for m in misreads {
        match out
            .iter_mut()
            .find(|o| o.raw == m.raw && o.fixed == m.fixed)
        {
            Some(o) => o.strong |= m.strong,
            None => out.push(m.clone()),
        }
    }
    out.iter().map(Misread::message).collect()
}

/// Byte offset of `token` in `line` where it isn't part of a longer number.
fn find_token(line: &str, token: &str) -> Option<usize> {
    let numeric = |c: char| c.is_ascii_digit() || c == '.' || c == ',';
    line.match_indices(token).map(|(at, _)| at).find(|&at| {
        let before = line[..at].chars().next_back();
        let after = line[at + token.len()..].chars().next();
        !before.is_some_and(numeric) && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

/// Every relation `x` (number `skip` on `line`) satisfies with numbers within
/// `window` lines: `x = a + b`, `b − a = x`, or an amount in words.
fn relations(
    x: i64,
    skip: usize,
    line: usize,
    window: usize,
    nums: &[Num],
    words: &[(usize, i64)],
) -> Vec<Evidence> {
    let near: Vec<(usize, &Num, i64)> = nums
        .iter()
        .enumerate()
        .filter(|(j, m)| *j != skip && m.line.abs_diff(line) <= window)
        .flat_map(|(j, m)| m.values.iter().map(move |&v| (j, m, v)))
        .filter(|(_, _, v)| *v != 0)
        .collect();
    let mut out = Vec::new();
    for (ia, a, va) in &near {
        for (ib, b, vb) in &near {
            // Three different lines: numbers on one line are often parts of a
            // code ("R147 C131"), and their arithmetic proves nothing.
            if ia == ib || a.line == b.line || a.line == line || b.line == line {
                continue;
            }
            if ia < ib && va + vb == x {
                out.push(Evidence {
                    text: format!("= {} + {}", a.raw, b.raw),
                    partners: vec![*ia, *ib],
                    words: false,
                });
            }
            if vb - va == x {
                out.push(Evidence {
                    text: format!("{} − {} = {}", b.raw, a.raw, show(x)),
                    partners: vec![*ia, *ib],
                    words: false,
                });
            }
        }
    }
    if words
        .iter()
        .any(|(l, w)| *w == x && l.abs_diff(line) <= window)
    {
        out.push(Evidence {
            text: "matches the amount written in words".into(),
            partners: Vec::new(),
            words: true,
        });
    }
    out
}

/// Numeric tokens on one line. Tokens touching `/`, `:` or `-` are dates,
/// times or IDs and are skipped, as are long integers. OCR often drops a
/// decimal point (`Q 30 46`): an integer followed by one space and exactly
/// two digits is read as one decimal number.
fn extract(line: &str, idx: usize) -> Vec<Num> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == ',') {
            i += 1;
        }
        let mut end = i;
        while end > start && !chars[end - 1].is_ascii_digit() {
            end -= 1;
        }
        let joined = |c: Option<&char>| c.is_some_and(|c| matches!(c, '/' | ':' | '-'));
        let before = start.checked_sub(1).and_then(|p| chars.get(p));
        if !joined(before) && !joined(chars.get(end)) {
            spans.push((start, end));
        }
    }
    let text = |(s, e): (usize, usize)| chars[s..e].iter().collect::<String>();
    let digits_only = |t: &str| t.chars().all(|c| c.is_ascii_digit());
    let mut out = Vec::new();
    let mut k = 0;
    while k < spans.len() {
        let glued = spans[k].0 > 0 && chars[spans[k].0 - 1].is_alphabetic();
        let mut raw = text(spans[k]);
        let mut span = raw.clone();
        if let Some(&next) = spans.get(k + 1) {
            let cents = text(next);
            if digits_only(&raw)
                && cents.len() == 2
                && digits_only(&cents)
                && next.0 == spans[k].1 + 1
                && chars[spans[k].1] == ' '
            {
                span = text((spans[k].0, next.1));
                raw = format!("{raw}.{cents}");
                k += 1;
            }
        }
        k += 1;
        let values = parse(&raw);
        if !values.is_empty() {
            out.push(Num {
                code: glued && digits_only(&raw),
                raw,
                span,
                values,
                line: idx,
            });
        }
    }
    out
}

/// Every plausible reading of a numeric token, scaled by `SCALE`. A lone
/// comma may be a decimal or a thousands separator.
fn parse(raw: &str) -> Vec<i64> {
    let dots = raw.matches('.').count();
    let commas = raw.matches(',').count();
    let mut readings = Vec::new();
    match (dots, commas) {
        (0, 0) => readings.push(raw.to_owned()),
        (1, 0) => readings.push(raw.to_owned()),
        (_, 0) => readings.push(raw.replace('.', "")),
        (0, 1) => {
            let after = raw.len() - raw.find(',').unwrap() - 1;
            if after <= 2 {
                readings.push(raw.replace(',', "."));
            }
            if after == 3 {
                readings.push(raw.replace(',', ""));
            }
        }
        (0, _) => readings.push(raw.replace(',', "")),
        _ => {
            // Both: the last separator is the decimal point.
            let last = raw.rfind(['.', ',']).unwrap();
            let (int, frac) = raw.split_at(last);
            readings.push(format!("{}.{}", int.replace(['.', ','], ""), &frac[1..]));
        }
    }
    readings
        .into_iter()
        .filter(|r| {
            r.split('.')
                .next()
                .is_some_and(|i| i.len() <= MAX_INTEGER_DIGITS)
        })
        .filter_map(|r| r.parse::<f64>().ok())
        .map(|v| (v * SCALE).round() as i64)
        .collect()
}

/// One-digit swaps along `CONFUSABLE`, both directions.
fn variants(raw: &str) -> Vec<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = Vec::new();
    for (i, c) in chars.iter().enumerate() {
        for &(a, b) in CONFUSABLE {
            let swap = if *c == a {
                b
            } else if *c == b {
                a
            } else {
                continue;
            };
            let mut v = chars.clone();
            v[i] = swap;
            out.push(v.into_iter().collect());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// An amount written in words on a line with no digits of its own:
/// "ochenta … cuarenta y seis centavos" → 80.46.
fn amount_in_words(line: &str) -> Option<i64> {
    if line.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let lower = line.to_lowercase();
    let converted = [
        text2num::replace_numbers_in_text(&lower, &text2num::Language::spanish(), 0.0),
        text2num::replace_numbers_in_text(&lower, &text2num::Language::english(), 0.0),
    ]
    .into_iter()
    .find(|c| c.chars().any(|ch| ch.is_ascii_digit()))?;
    let found: Vec<i64> = extract(&converted, 0)
        .into_iter()
        .filter_map(|n| n.values.first().copied())
        .collect();
    // text2num renders "centavos" as a trailing `1/100` fraction.
    let subunit = converted.contains("1/100") || SUBUNIT_WORDS.iter().any(|w| lower.contains(w));
    match found.as_slice() {
        [whole, cents] if subunit && *cents < 100 * SCALE as i64 => Some(whole + cents / 100),
        [whole] => Some(*whole),
        _ => None,
    }
}

fn significant_digits(raw: &str) -> usize {
    raw.chars()
        .filter(char::is_ascii_digit)
        .skip_while(|c| *c == '0')
        .count()
}

fn show(v: i64) -> String {
    let s = format!("{:.5}", v as f64 / SCALE);
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(text: &str) -> Vec<String> {
        correct(text).1
    }

    const FADED_BILL: &str = "\
Concepto Precio Total
Servicio A Q 70.46
Cargo fijo Q 10
Total a Pagar Q 30.46
Total en letras
Ochenta Quetzales Con Cuarenta y Seis Centavos
Lectura Anterior 5114
Lectura Actual 5104 Fecha 26/08/2026
Consumo 80";

    #[test]
    fn strong_evidence_corrects_and_a_single_relation_only_reports() {
        let (text, checks, applied) = correct(FADED_BILL);
        assert_eq!(applied, 1);
        assert!(
            text.contains("Total a Pagar Q 80.46 [corrected from 30.46]"),
            "{text}"
        );
        assert!(
            text.contains("Lectura Actual 5104 Fecha"),
            "weak evidence leaves the text alone"
        );
        assert_eq!(checks.len(), 2, "{checks:#?}");
        assert!(
            checks[0].starts_with("\"30.46\" was corrected to 80.46"),
            "{}",
            checks[0]
        );
        assert!(checks[0].contains("= 70.46 + 10"), "{}", checks[0]);
        assert!(
            checks[0].contains("amount written in words"),
            "{}",
            checks[0]
        );
        assert!(
            checks[1].starts_with("\"5104\" may be a misread of 5194"),
            "{}",
            checks[1]
        );
        assert!(checks[1].contains("5114 + 80"), "{}", checks[1]);
    }

    #[test]
    fn every_occurrence_is_corrected_including_a_dropped_decimal_point() {
        let doc = "Item Q 70.46\nFee Q 10\nSubtotal Q 30 46\nTotal Q 30.46\nOchenta con cuarenta y seis centavos";
        let (text, checks, applied) = correct(doc);
        assert_eq!(applied, 2, "{text}");
        assert!(
            text.contains("Subtotal Q 80.46 [corrected from 30 46]"),
            "{text}"
        );
        assert!(
            text.contains("Total Q 80.46 [corrected from 30.46]"),
            "{text}"
        );
        assert_eq!(
            checks.len(),
            1,
            "one message per distinct misread: {checks:#?}"
        );
    }

    #[test]
    fn two_relations_with_disjoint_partners_correct() {
        // 3500 fits nothing; 8500 = 6100 + 2400 and = 9200 − 700.
        let doc = "A 6100\nB 2400\nC 3500\nD 9200\nE 700";
        let (text, _, applied) = correct(doc);
        assert_eq!(applied, 1, "{text}");
        assert!(text.contains("C 8500 [corrected from 3500]"), "{text}");
    }

    #[test]
    fn a_span_not_found_as_a_whole_token_is_left_alone() {
        let mut m = vec![Misread {
            line: 0,
            span: "30.46".into(),
            raw: "30.46".into(),
            fixed: "80.46".into(),
            why: vec!["x".into()],
            strong: true,
        }];
        let text = apply_corrections("Total 130.46", &mut m);
        assert_eq!(text, "Total 130.46");
        assert!(!m[0].strong, "downgraded to a report");
        assert!(m[0].message().contains("may be a misread of"));
    }

    #[test]
    fn a_consistent_document_yields_nothing() {
        let ok = "Item 70.46\nFee 10\nTotal 80.46\nPrevious 5114\nCurrent 5194\nUsed 80";
        assert_eq!(correct(ok), (ok.to_owned(), Vec::new(), 0));
    }

    #[test]
    fn comma_decimals_and_english_words_are_understood() {
        let doc = "Item 70,46\nFee 10\nTotal 30,46\nEighty dollars and forty-six cents";
        let checks = checks(doc);
        assert_eq!(checks.len(), 1, "{checks:#?}");
        assert!(checks[0].contains("corrected to 80,46"), "{}", checks[0]);
        assert!(
            checks[0].contains("amount written in words"),
            "{}",
            checks[0]
        );
    }

    #[test]
    fn dates_times_ids_and_small_numbers_are_ignored() {
        let doc = "Date 05/09/2026 at 10:30\nNIT 112233-4\nRef 123456789\nA 46\nB 30\nC 10";
        assert!(extract("Date 05/09/2026 at 10:30", 0).is_empty());
        assert!(extract("NIT 112233-4", 0).is_empty());
        assert_eq!(parse("123456789"), Vec::<i64>::new());
        // 46 → 40 = 30 + 10 would be a coincidence; 46 is too short to repair.
        assert!(checks(doc).is_empty());
    }

    #[test]
    fn ambiguous_repairs_are_dropped() {
        // 130 → 180 (= 80 + 100) and 130 → 730 (= 100 + 630) both fit.
        let doc = "A 80\nB 100\nC 630\nX 130";
        let checks = checks(doc);
        assert!(
            !checks.iter().any(|c| c.starts_with("\"130\"")),
            "{checks:#?}"
        );
    }

    #[test]
    fn a_dropped_decimal_point_is_restored() {
        let raws: Vec<String> = extract("Total Q 30 46", 0)
            .into_iter()
            .map(|n| n.raw)
            .collect();
        assert_eq!(raws, vec!["30.46"]);
        let raws: Vec<String> = extract("A 30 460", 0).into_iter().map(|n| n.raw).collect();
        assert_eq!(raws, vec!["30", "460"]);
    }

    #[test]
    fn relations_need_three_different_lines() {
        // 147 → 141 = 131 + 10 would use two numbers from one code line.
        let doc = "Ruta R147 C131\nFee 10";
        assert!(checks(doc).is_empty(), "{:#?}", checks(doc));
    }

    #[test]
    fn codes_glued_to_letters_are_never_repaired() {
        // 131 → 137 = 147 − 10 fits, but C131 is a code. (Codes may still be
        // partners: `Q10` is a glued amount on real OCR output.)
        let doc = "Ruta 147\nAddress C131\nFee 10";
        let found = checks(doc);
        assert!(
            !found.iter().any(|c| c.starts_with("\"131\"")),
            "{found:#?}"
        );
        // A glued amount with cents is still an amount.
        let doc = "Item 70.46\nFee 10\nTotal Q30.46";
        assert_eq!(checks(doc).len(), 1);
    }

    #[test]
    fn number_parsing_handles_separators() {
        assert_eq!(parse("80.46"), vec![8_046_000]);
        assert_eq!(parse("1,234.50"), vec![123_450_000]);
        assert_eq!(parse("1.234,50"), vec![123_450_000]);
        assert_eq!(parse("7,55"), vec![755_000]);
        assert_eq!(parse("1,234"), vec![123_400_000]);
        assert_eq!(show(8_046_000), "80.46");
        assert_eq!(show(519_400_000), "5194");
    }

    #[test]
    fn amounts_in_words_combine_whole_and_subunit() {
        assert_eq!(
            amount_in_words("Ochenta Quetzales Con Cuarenta y Seis Centavos"),
            Some(8_046_000)
        );
        assert_eq!(amount_in_words("Total en letras"), None);
        assert_eq!(
            amount_in_words("Total 80.46"),
            None,
            "lines with digits are not word amounts"
        );
    }
}
