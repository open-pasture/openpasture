//! What a text says, read strictly: a command is the whole text (case and
//! surrounding spaces or a closing `.`, `!` or `?` aside), so "no thanks" is
//! a question, never a rejection.
//!
//! - `y` `yes` `si` `sí` `approve` / `n` `no` `reject`, optionally followed
//!   by the decision's 4-digit code ("Y 4821") or a list number ("Y 2")
//! - `later` (optionally with a code or number), `ok`, `status`
//! - `where is <tag>`, `where <tag>`
//! - `stop move` (optionally with a code or number)
//! - Twilio's own keywords are mirrored, never commands of ours: STOP,
//!   STOPALL, UNSUBSCRIBE, CANCEL, END, QUIT, REVOKE, OPTOUT opt the number
//!   out; START and UNSTOP opt it in (so does YES, for a number that opted
//!   out); HELP and INFO get Twilio's answer and none from us.
//! - six digits: a verification code texted back
//! - anything else is a question.

/// Which decision (or move) a command means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// The only one there is (or ask which, with a numbered list).
    Only,
    /// The 4-digit code from the decision's text.
    Code(String),
    /// A number from the list we texted.
    Number(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Approve (`true`) or reject a waiting decision.
    Answer {
        approve: bool,
        pick: Pick,
    },
    /// Ask again in an hour.
    Later(Pick),
    /// Ack the last alert texted to this person.
    Ack,
    Status,
    /// Where an animal is, by tag (as typed).
    Where(String),
    StopMove(Pick),
    OptOut,
    OptIn,
    /// HELP, INFO: Twilio answers.
    Reserved,
    /// Six digits: a phone verification code.
    Code(String),
    Question(String),
}

const APPROVE: [&str; 5] = ["y", "yes", "si", "sí", "approve"];
const REJECT: [&str; 3] = ["n", "no", "reject"];
const OPT_OUT: [&str; 8] = ["stop", "stopall", "unsubscribe", "cancel", "end", "quit", "revoke", "optout"];
const OPT_IN: [&str; 2] = ["start", "unstop"];
const RESERVED: [&str; 2] = ["help", "info"];

/// Lowercased, spaces collapsed, a closing `.`, `!` or `?` dropped.
fn norm(text: &str) -> String {
    let t = text.trim().trim_end_matches(['.', '!', '?']).trim();
    t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A trailing code or list number: `[]` → only, 4 digits → code, 1 or 2 digits → number.
fn pick(rest: &[&str]) -> Option<Pick> {
    match rest {
        [] => Some(Pick::Only),
        [n] if n.bytes().all(|b| b.is_ascii_digit()) => match n.len() {
            4 => Some(Pick::Code((*n).to_owned())),
            1 | 2 => n.parse::<usize>().ok().filter(|i| *i > 0).map(Pick::Number),
            _ => None,
        },
        _ => None,
    }
}

/// "y4821" → ["y", "4821"]: a word run straight into its code.
fn split_glued(tokens: Vec<&str>) -> Vec<String> {
    if let [one] = tokens.as_slice()
        && let Some(i) = one.find(|c: char| c.is_ascii_digit())
        && i > 0
        && one[i..].bytes().all(|b| b.is_ascii_digit())
    {
        return vec![one[..i].to_owned(), one[i..].to_owned()];
    }
    tokens.into_iter().map(str::to_owned).collect()
}

pub fn parse(text: &str) -> Command {
    let low = norm(text);
    let question = || Command::Question(text.trim().to_owned());
    if low.is_empty() {
        return question();
    }
    let digits: String = low.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit()) {
        return Command::Code(digits);
    }
    if OPT_OUT.contains(&low.as_str()) {
        return Command::OptOut;
    }
    if OPT_IN.contains(&low.as_str()) {
        return Command::OptIn;
    }
    if RESERVED.contains(&low.as_str()) {
        return Command::Reserved;
    }
    match low.as_str() {
        "ok" => return Command::Ack,
        "status" => return Command::Status,
        _ => {}
    }
    // "where is 214", "where 214": the tag as typed.
    let words: Vec<&str> = text.trim().trim_end_matches(['.', '!', '?']).split_whitespace().collect();
    if words.first().is_some_and(|w| w.eq_ignore_ascii_case("where")) {
        let rest = if words.get(1).is_some_and(|w| w.eq_ignore_ascii_case("is")) { &words[2..] } else { &words[1..] };
        return if rest.is_empty() { question() } else { Command::Where(rest.join(" ")) };
    }
    let tokens = split_glued(low.split_whitespace().collect());
    let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
    match tokens.as_slice() {
        ["stop", "move", rest @ ..] => pick(rest).map_or_else(question, Command::StopMove),
        ["later", rest @ ..] => pick(rest).map_or_else(question, Command::Later),
        [w, rest @ ..] if APPROVE.contains(w) => pick(rest).map_or_else(question, |p| Command::Answer { approve: true, pick: p }),
        [w, rest @ ..] if REJECT.contains(w) => pick(rest).map_or_else(question, |p| Command::Answer { approve: false, pick: p }),
        _ => question(),
    }
}

/// The words Twilio takes as opting back in (YES among them).
pub fn twilio_opt_in(text: &str) -> bool {
    matches!(norm(text).as_str(), "start" | "unstop" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(approve: bool, pick: Pick) -> Command {
        Command::Answer { approve, pick }
    }

    #[test]
    fn answers_with_and_without_codes() {
        for t in ["y", "Y", " yes ", "YES!", "si", "Sí", "SÍ", "approve", "Approve."] {
            assert_eq!(parse(t), answer(true, Pick::Only), "{t}");
        }
        for t in ["n", "No", "reject", "NO."] {
            assert_eq!(parse(t), answer(false, Pick::Only), "{t}");
        }
        assert_eq!(parse("Y 4821"), answer(true, Pick::Code("4821".into())));
        assert_eq!(parse("y4821"), answer(true, Pick::Code("4821".into())));
        assert_eq!(parse("n  0042"), answer(false, Pick::Code("0042".into())));
        assert_eq!(parse("y 2"), answer(true, Pick::Number(2)));
        assert_eq!(parse("si 12"), answer(true, Pick::Number(12)));
        // Not commands: read as questions.
        for t in ["no thanks", "y 0", "y 123", "y 48210", "yes please do", "yy"] {
            assert!(matches!(parse(t), Command::Question(_)), "{t}");
        }
    }

    #[test]
    fn the_other_commands() {
        assert_eq!(parse("OK"), Command::Ack);
        assert_eq!(parse("status?"), Command::Status);
        assert_eq!(parse("later"), Command::Later(Pick::Only));
        assert_eq!(parse("Later 4821"), Command::Later(Pick::Code("4821".into())));
        assert_eq!(parse("where is 214"), Command::Where("214".into()));
        assert_eq!(parse("Where 214?"), Command::Where("214".into()));
        assert_eq!(parse("where is  Red   Cow"), Command::Where("Red Cow".into()));
        assert_eq!(parse("stop move"), Command::StopMove(Pick::Only));
        assert_eq!(parse("STOP MOVE 1"), Command::StopMove(Pick::Number(1)));
        assert_eq!(parse("stop move 4821"), Command::StopMove(Pick::Code("4821".into())));
        assert!(matches!(parse("where"), Command::Question(_)));
        assert!(matches!(parse("How much grass is in P3?"), Command::Question(_)));
    }

    #[test]
    fn twilio_keywords_are_mirrored_never_ours() {
        for t in ["STOP", "stop", "Stop.", "stopall", "unsubscribe", "cancel", "end", "quit", "revoke", "optout"] {
            assert_eq!(parse(t), Command::OptOut, "{t}");
        }
        assert_eq!(parse("START"), Command::OptIn);
        assert_eq!(parse("unstop"), Command::OptIn);
        assert_eq!(parse("help"), Command::Reserved);
        assert_eq!(parse("INFO"), Command::Reserved);
        assert!(twilio_opt_in("Yes") && twilio_opt_in("START") && !twilio_opt_in("y") && !twilio_opt_in("si"));
    }

    #[test]
    fn six_digits_are_a_code() {
        assert_eq!(parse("123456"), Command::Code("123456".into()));
        assert_eq!(parse(" 123 456 "), Command::Code("123456".into()));
        assert_eq!(parse("123-456"), Command::Code("123456".into()));
        assert!(matches!(parse("12345"), Command::Question(_)));
    }
}
