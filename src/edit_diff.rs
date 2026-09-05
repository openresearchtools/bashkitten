//! Pinned Pi edit-diff.ts and its jsdiff 8.0.4 dependency's default line diff
//! and FILE_HEADERS_ONLY patch generation. jsdiff's MIT notice is retained.
use std::{collections::HashMap, rc::Rc};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Equal,
    Add,
    Remove,
}
struct Component {
    kind: Kind,
    count: usize,
    previous: Option<Rc<Component>>,
}
#[derive(Clone)]
struct DiffPath {
    old: isize,
    tail: Option<Rc<Component>>,
}
struct Part<'a> {
    kind: Kind,
    lines: Vec<&'a str>,
}

impl DiffPath {
    fn push(&mut self, kind: Kind, count: usize) {
        if count == 0 {
            return;
        }
        let (count, previous) = match &self.tail {
            Some(last) if last.kind == kind => (last.count + count, last.previous.clone()),
            _ => (count, self.tail.clone()),
        };
        self.tail = Some(Rc::new(Component {
            kind,
            count,
            previous,
        }));
    }
    fn common(&mut self, old: &[&str], new: &[&str], diagonal: isize) -> isize {
        let mut next = self.old - diagonal;
        let mut count = 0;
        while self.old + 1 < old.len() as isize
            && next + 1 < new.len() as isize
            && old[(self.old + 1) as usize] == new[(next + 1) as usize]
        {
            self.old += 1;
            next += 1;
            count += 1;
        }
        self.push(Kind::Equal, count);
        next
    }
}
fn diff<'a>(old: &'a str, new: &'a str) -> Vec<Part<'a>> {
    let old: Vec<_> = old.split_inclusive('\n').collect();
    let new: Vec<_> = new.split_inclusive('\n').collect();
    let mut initial = DiffPath {
        old: -1,
        tail: None,
    };
    let next = initial.common(&old, &new, 0);
    let finished = |path: &DiffPath, next: isize| {
        path.old + 1 >= old.len() as isize && next + 1 >= new.len() as isize
    };
    let tail = if finished(&initial, next) {
        initial.tail
    } else {
        let mut paths = HashMap::from([(0_isize, initial)]);
        let (mut min, mut max) = (isize::MIN, isize::MAX);
        let mut result = None;
        'search: for distance in 1..=(old.len() + new.len()) as isize {
            for diagonal in (min.max(-distance)..=max.min(distance)).step_by(2) {
                let remove = paths.remove(&(diagonal - 1));
                let add = paths.get(&(diagonal + 1));
                let can_add = add.is_some_and(|path| {
                    let n = path.old - diagonal;
                    n >= 0 && n < new.len() as isize
                });
                let can_remove = remove
                    .as_ref()
                    .is_some_and(|path| path.old + 1 < old.len() as isize);
                if !can_add && !can_remove {
                    paths.remove(&diagonal);
                    continue;
                }
                let mut base = if !can_remove
                    || (can_add && remove.as_ref().unwrap().old < add.unwrap().old)
                {
                    let mut base = add.unwrap().clone();
                    base.push(Kind::Add, 1);
                    base
                } else {
                    let mut base = remove.unwrap();
                    base.old += 1;
                    base.push(Kind::Remove, 1);
                    base
                };
                let next = base.common(&old, &new, diagonal);
                if finished(&base, next) {
                    result = base.tail;
                    break 'search;
                }
                if base.old + 1 >= old.len() as isize {
                    max = max.min(diagonal - 1);
                }
                if next + 1 >= new.len() as isize {
                    min = min.max(diagonal + 1);
                }
                paths.insert(diagonal, base);
            }
        }
        result
    };
    let mut components = Vec::new();
    let mut current = tail;
    while let Some(part) = current {
        components.push((part.kind, part.count));
        current = part.previous.clone();
    }
    components.reverse();
    let (mut a, mut b) = (0, 0);
    components
        .into_iter()
        .map(|(kind, count)| {
            let lines = if kind == Kind::Remove {
                old[a..a + count].to_vec()
            } else {
                new[b..b + count].to_vec()
            };
            if kind != Kind::Add {
                a += count;
            }
            if kind != Kind::Remove {
                b += count;
            }
            Part { kind, lines }
        })
        .collect()
}

pub fn unified_patch(path: &str, old: &str, new: &str) -> String {
    let mut parts = diff(old, new);
    parts.push(Part {
        kind: Kind::Equal,
        lines: vec![],
    });
    let mut output = format!("--- {path}\n+++ {path}\n");
    let (mut old_line, mut new_line, mut old_start, mut new_start) =
        (1_usize, 1_usize, 0_usize, 0_usize);
    let mut range: Vec<(char, &str)> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if part.kind != Kind::Equal {
            if old_start == 0 {
                old_start = old_line;
                new_start = new_line;
                if i > 0 {
                    let prev = &parts[i - 1].lines;
                    let start = prev.len().saturating_sub(4);
                    range = prev[start..].iter().map(|line| (' ', *line)).collect();
                    old_start -= range.len();
                    new_start -= range.len();
                }
            }
            range.extend(
                part.lines
                    .iter()
                    .map(|line| (if part.kind == Kind::Add { '+' } else { '-' }, *line)),
            );
            if part.kind == Kind::Add {
                new_line += part.lines.len();
            } else {
                old_line += part.lines.len();
            }
        } else {
            if old_start != 0 {
                if part.lines.len() <= 8 && i < parts.len() - 2 {
                    range.extend(part.lines.iter().map(|line| (' ', *line)));
                } else {
                    let context = part.lines.len().min(4);
                    range.extend(part.lines[..context].iter().map(|line| (' ', *line)));
                    let old_count = old_line - old_start + context;
                    let new_count = new_line - new_start + context;
                    output.push_str(&format!(
                        "@@ -{},{} +{},{} @@\n",
                        old_start - usize::from(old_count == 0),
                        old_count,
                        new_start - usize::from(new_count == 0),
                        new_count
                    ));
                    for (prefix, line) in range.drain(..) {
                        output.push(prefix);
                        output.push_str(line);
                        if !line.ends_with('\n') {
                            output.push_str("\n\\ No newline at end of file\n");
                        }
                    }
                    old_start = 0;
                    new_start = 0;
                }
            }
            old_line += part.lines.len();
            new_line += part.lines.len();
        }
    }
    output
}

pub fn display_diff(old: &str, new: &str) -> (String, Option<usize>) {
    let parts = diff(old, new);
    let width = old
        .split('\n')
        .count()
        .max(new.split('\n').count())
        .to_string()
        .len();
    let (mut old_line, mut new_line) = (1, 1);
    let mut first = None;
    let mut output = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if part.kind != Kind::Equal {
            first.get_or_insert(new_line);
            for line in &part.lines {
                let line = line.strip_suffix('\n').unwrap_or(line);
                if part.kind == Kind::Add {
                    output.push(format!("+{new_line:>width$} {line}"));
                    new_line += 1;
                } else {
                    output.push(format!("-{old_line:>width$} {line}"));
                    old_line += 1;
                }
            }
        } else {
            let leading = i > 0 && parts[i - 1].kind != Kind::Equal;
            let trailing = i + 1 < parts.len() && parts[i + 1].kind != Kind::Equal;
            let len = part.lines.len();
            let mut skipped = false;
            for (j, line) in part.lines.iter().enumerate() {
                let show = (leading && j < 4)
                    || (trailing && j >= len.saturating_sub(4))
                    || (leading && trailing && len <= 8);
                if show {
                    output.push(format!(
                        " {old_line:>width$} {}",
                        line.strip_suffix('\n').unwrap_or(line)
                    ));
                } else if !skipped && (leading || trailing) {
                    output.push(format!(" {} ...", " ".repeat(width)));
                    skipped = true;
                }
                old_line += 1;
                new_line += 1;
            }
        }
    }
    (output.join("\n"), first)
}
