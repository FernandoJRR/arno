//! Typed requests to see the approval buttons (SPEC §11 #26). A match only
//! re-shows an already frozen decision; it never approves (SPEC §11 #18).

/// At least one of these must appear.
const INTENT: &[&str] = &[
    "approve",
    "approved",
    "approval",
    "confirm",
    "confirmed",
    "confirmation",
    "yes",
    "ok",
    "okay",
    "button",
    "buttons",
    "aprobar",
    "apruebo",
    "aprobado",
    "confirmar",
    "confirmo",
    "si",
    "sí",
    "dale",
    "botón",
    "boton",
    "botones",
];

/// Words allowed around the intent; anything else ("but", "change",
/// "category") means the message is a new request for the model.
const FILLER: &[&str] = &[
    "i",
    "want",
    "to",
    "show",
    "me",
    "the",
    "a",
    "it",
    "this",
    "that",
    "please",
    "pls",
    "now",
    "again",
    "give",
    "send",
    "let",
    "do",
    "go",
    "ahead",
    "can",
    "you",
    "so",
    "decision",
    "and",
    "quiero",
    "muéstrame",
    "muestrame",
    "muestra",
    "el",
    "la",
    "los",
    "las",
    "por",
    "favor",
    "dame",
    "de",
    "otra",
    "vez",
    "ahora",
    "lo",
    "para",
    "envía",
    "envia",
    "ok",
    "okay",
];

/// "Approve", "Okay show me to approve", "Show me the approval buttons".
pub fn asks_for_decision(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let go_ahead = lower.contains("go ahead") || lower.contains("do it");
    !words.is_empty()
        && words.len() <= 8
        && words
            .iter()
            .all(|w| INTENT.contains(w) || FILLER.contains(w))
        && (go_ahead || words.iter().any(|w| INTENT.contains(w)))
}

/// Longer messages are new requests, not changes to a proposal.
const MAX_AMEND_WORDS: usize = 12;

/// The parameters a short message names by their stem ("add a category" →
/// `category_id`): a change to the cancelled proposal (SPEC §11 #26).
pub fn amend_targets<'a>(text: &str, params: impl Iterator<Item = &'a str>) -> Vec<String> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() || words.len() > MAX_AMEND_WORDS {
        return Vec::new();
    }
    params
        .filter(|p| super::args::stem(p).is_some_and(|s| words.iter().any(|w| w.starts_with(s))))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: [&str; 5] = ["amount", "account_id", "category_id", "date", "apply"];

    fn targets(text: &str) -> Vec<String> {
        amend_targets(text, PARAMS.into_iter())
    }

    #[test]
    fn short_messages_naming_a_parameter_are_amendments() {
        assert_eq!(targets("Add a category"), ["category_id"]);
        assert_eq!(targets("change the date to the 16th"), ["date"]);
        assert_eq!(targets("ponle categoría"), ["category_id"]);
    }

    #[test]
    fn new_requests_and_chatter_are_not() {
        assert!(
            targets("I want to add a 181 gtq expense for Pollo Campero today with a category")
                .is_empty()
        );
        assert!(targets("hello").is_empty());
        assert!(targets("").is_empty());
    }

    #[test]
    fn short_requests_for_the_buttons_match() {
        for t in [
            "Approve",
            "approve.",
            "Okay show me to approve",
            "Show me the approval buttons decision",
            "yes",
            "Go ahead",
            "Sí, dale",
            "muéstrame los botones",
        ] {
            assert!(asks_for_decision(t), "{t}");
        }
    }

    #[test]
    fn requests_with_changes_or_new_content_do_not() {
        for t in [
            "Add a category of the ones that already exist",
            "yes but change the date",
            "Show me",
            "That's it",
            "",
            "I want to add a 181 gtq expense",
        ] {
            assert!(!asks_for_decision(t), "{t}");
        }
    }
}
