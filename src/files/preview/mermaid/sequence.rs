use std::collections::HashMap;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::canvas::Canvas;
use super::glyphs::Glyphs;
use super::routing;

const MAX_PARTICIPANTS: usize = 24;
const MAX_MESSAGES: usize = 256;
const MAX_LABEL: usize = 100;

#[derive(Clone, Debug)]
pub struct Participant {
    pub label: String,
    pub actor: bool,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub from: usize,
    pub to: usize,
    pub label: String,
    pub dotted: bool,
}

#[derive(Clone, Debug)]
pub struct Note {
    pub participant: usize,
    pub text: String,
}

#[derive(Clone, Debug)]
pub enum SequenceEvent {
    Message(Message),
    Note(Note),
}

#[derive(Clone, Debug)]
pub struct Sequence {
    pub participants: Vec<Participant>,
    pub events: Vec<SequenceEvent>,
}

pub fn parse(source: &str) -> Result<Sequence, String> {
    let mut lines = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("%%"));
    if !lines
        .next()
        .is_some_and(|line| line.eq_ignore_ascii_case("sequenceDiagram"))
    {
        return Err("expected sequenceDiagram".into());
    }
    let mut participants = Vec::<Participant>::new();
    let mut ids = HashMap::<String, usize>::new();
    let mut events = Vec::new();
    for line in lines {
        if line.starts_with("participant ") || line.starts_with("actor ") {
            let actor = line.starts_with("actor ");
            let declaration = line
                .split_once(' ')
                .map(|(_, rest)| rest)
                .unwrap_or_default();
            let (id, label) = declaration
                .split_once(" as ")
                .map(|(id, label)| (id.trim(), label.trim()))
                .unwrap_or((declaration.trim(), declaration.trim()));
            ensure_participant(id, Some(label), actor, &mut participants, &mut ids)?;
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("Note ")
            .or_else(|| line.strip_prefix("note "))
        {
            let Some((target, text)) = rest.split_once(':') else {
                return Err("a sequence note needs ':' text".into());
            };
            let target = target
                .split_whitespace()
                .last()
                .unwrap_or_default()
                .split(',')
                .next()
                .unwrap_or_default();
            let id = ensure_participant(target, None, false, &mut participants, &mut ids)?;
            events.push(SequenceEvent::Note(Note {
                participant: id,
                text: text.trim().chars().take(MAX_LABEL).collect(),
            }));
            continue;
        }
        if matches!(
            line.split_whitespace().next(),
            Some("activate" | "deactivate")
        ) {
            continue;
        }
        let Some((left, label)) = line.split_once(':') else {
            return Err(format!("unsupported sequence statement {line:?}"));
        };
        let Some((from, to, dotted)) = split_message(left.trim()) else {
            return Err(format!("unsupported sequence message {left:?}"));
        };
        let from = ensure_participant(from, None, false, &mut participants, &mut ids)?;
        let to = ensure_participant(to, None, false, &mut participants, &mut ids)?;
        events.push(SequenceEvent::Message(Message {
            from,
            to,
            label: label.trim().chars().take(MAX_LABEL).collect(),
            dotted,
        }));
        if events.len() > MAX_MESSAGES {
            return Err(format!("diagram exceeds {MAX_MESSAGES} messages"));
        }
    }
    Ok(Sequence {
        participants,
        events,
    })
}

fn ensure_participant(
    id: &str,
    label: Option<&str>,
    actor: bool,
    participants: &mut Vec<Participant>,
    ids: &mut HashMap<String, usize>,
) -> Result<usize, String> {
    let id = id.trim();
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(format!("invalid participant {id:?}"));
    }
    if let Some(index) = ids.get(id).copied() {
        return Ok(index);
    }
    if participants.len() >= MAX_PARTICIPANTS {
        return Err(format!("diagram exceeds {MAX_PARTICIPANTS} participants"));
    }
    let index = participants.len();
    ids.insert(id.to_string(), index);
    participants.push(Participant {
        label: label.unwrap_or(id).chars().take(MAX_LABEL).collect(),
        actor,
    });
    Ok(index)
}

fn split_message(text: &str) -> Option<(&str, &str, bool)> {
    for (operator, dotted) in [
        ("-->>", true),
        ("->>", false),
        ("-->", true),
        ("->", false),
        ("--x", true),
        ("->x", false),
    ] {
        if let Some(index) = text.find(operator) {
            return Some((
                text[..index].trim(),
                text[index + operator.len()..].trim(),
                dotted,
            ));
        }
    }
    None
}

pub fn render(sequence: &Sequence, width: usize, ascii: bool) -> Vec<String> {
    if sequence.participants.is_empty() {
        return vec!["empty sequence diagram".into()];
    }
    let slot = sequence
        .participants
        .iter()
        .map(|p| p.label.width() + 6)
        .max()
        .unwrap_or(10)
        .max(10);
    let required = slot * sequence.participants.len();
    if required > width || width < 24 {
        return render_outline(sequence, width, ascii);
    }
    let glyphs = Glyphs::for_ascii(ascii);
    let height = 3 + sequence.events.len() * 2 + 1;
    // Extra room preserves message labels in a wide pane, but keep the canvas
    // bounded by the largest supported label rather than an arbitrary client width.
    let canvas_width = width.min(required.saturating_add(MAX_LABEL));
    let mut canvas = Canvas::new(canvas_width, height);
    let centers: Vec<usize> = (0..sequence.participants.len())
        .map(|index| index * slot + slot / 2)
        .collect();
    for (index, participant) in sequence.participants.iter().enumerate() {
        let label = if participant.actor {
            format!("({})", participant.label)
        } else {
            format!("[{}]", participant.label)
        };
        let x = centers[index].saturating_sub(label.width() / 2);
        canvas.write(x, 0, &label);
        canvas.vline(centers[index], 1, height - 1, glyphs.vertical);
    }
    for (event_index, event) in sequence.events.iter().enumerate() {
        let y = 2 + event_index * 2;
        match event {
            SequenceEvent::Message(message) => {
                let from = centers[message.from];
                let to = centers[message.to];
                routing::horizontal_arrow(&mut canvas, from, to, y, message.dotted, glyphs);
                let label_width = message.label.width();
                let midpoint = (from + to) / 2;
                canvas.write(
                    midpoint.saturating_sub(label_width / 2),
                    y.saturating_sub(1),
                    &message.label,
                );
            }
            SequenceEvent::Note(note) => {
                let text = format!("note: {}", note.text);
                canvas.write(centers[note.participant].saturating_add(2), y, &text);
            }
        }
    }
    canvas.into_lines()
}

fn render_outline(sequence: &Sequence, width: usize, ascii: bool) -> Vec<String> {
    let arrow = if ascii { "->" } else { "→" };
    sequence
        .events
        .iter()
        .map(|event| match event {
            SequenceEvent::Message(message) => clip(
                &format!(
                    "{} {arrow} {} · {}",
                    sequence.participants[message.from].label,
                    sequence.participants[message.to].label,
                    message.label
                ),
                width,
            ),
            SequenceEvent::Note(note) => clip(
                &format!(
                    "{} · note: {}",
                    sequence.participants[note.participant].label, note.text
                ),
                width,
            ),
        })
        .collect()
}

fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|ch| {
            let next = used + ch.width().unwrap_or(0);
            if next > width {
                false
            } else {
                used = next;
                true
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_uses_a_compact_outline_when_narrow() {
        let sequence = parse("sequenceDiagram\n participant A as Alice\n A->>B: Hello").unwrap();
        let rendered = render(&sequence, 20, false);
        assert_eq!(rendered, vec!["Alice → B · Hello"]);
    }

    #[test]
    fn wide_panes_keep_long_message_labels_visible() {
        let label = "message label that needs the available pane width";
        let sequence = parse(&format!("sequenceDiagram\n A->>B: {label}")).unwrap();
        let rendered = render(&sequence, 80, false).join("\n");

        assert!(rendered.contains(label));
    }
}
