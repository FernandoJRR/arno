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

#[cfg(test)]
mod tests {
    use super::*;

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
