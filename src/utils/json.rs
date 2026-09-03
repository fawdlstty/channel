use serde_json::Value;

pub(crate) trait JsonValueExt {
    fn value_text(&self) -> String;
    fn string_value(&self) -> Option<String>;
    fn line_number(&self, keys: &[&str]) -> Option<usize>;
    fn file_path(&self) -> Option<String>;
    fn inferred_command_path(&self, name: &str) -> Option<String>;
}

impl JsonValueExt for Value {
    fn value_text(&self) -> String {
        self.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| self.to_string())
    }

    fn string_value(&self) -> Option<String> {
        match self {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
    }

    fn line_number(&self, keys: &[&str]) -> Option<usize> {
        let object = self.as_object()?;
        keys.iter()
            .find_map(|key| object.get(*key).and_then(Value::as_u64).map(|n| n as usize))
    }

    fn file_path(&self) -> Option<String> {
        let value = self.as_object()?;
        [
            "path",
            "file_path",
            "filePath",
            "filename",
            "file",
            "target",
            "uri",
        ]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).map(str::to_owned))
    }

    fn inferred_command_path(&self, name: &str) -> Option<String> {
        let name = name.to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "cat" | "grep" | "rg" | "ripgrep" | "read" | "read_file" | "readfile"
        ) {
            return None;
        }
        let command = self.as_str()?;
        command
            .split_whitespace()
            .find(|part| {
                part.starts_with("./")
                    || part.starts_with("../")
                    || part.starts_with('/')
                    || part.contains('.')
            })
            .map(|part| part.trim_matches('"').trim_matches('\'').to_owned())
    }
}
