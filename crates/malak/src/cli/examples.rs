// Cargo packages examples.md as a regular file containing the linked guide.
const GUIDE: &str = include_str!("../../examples.md");

pub(crate) fn snippets() -> impl Iterator<Item = (&'static str, &'static str)> {
    GUIDE.split("\n## `malak ").skip(1).map(|section| {
        let (command, section) = section.split_once("`\n").expect("example command");
        let (_, code) = section.split_once("\n```sh\n").expect("example code block");
        let (snippet, _) = code.split_once("\n```").expect("end of example");
        (command, snippet)
    })
}

pub(crate) fn snippet(command: &str) -> &'static str {
    snippets()
        .find_map(|(name, code)| (name == command).then_some(code))
        .unwrap_or_else(|| panic!("missing example for malak {command}"))
}

pub(super) fn help(commands: &[&str]) -> String {
    let snippets = commands
        .iter()
        .map(|command| snippet(command))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("Example:\n{snippets}")
}
