use std::path::PathBuf;

pub(crate) trait CommandPath {
    fn find_in_path(&self) -> Option<PathBuf>;
}

impl CommandPath for str {
    fn find_in_path(&self) -> Option<PathBuf> {
        if self.contains('/') {
            let path = PathBuf::from(self);
            return path.is_file().then_some(path);
        }
        let paths = std::env::var_os("PATH")?;
        std::env::split_paths(&paths)
            .map(|directory| directory.join(self))
            .find(|path| path.is_file())
    }
}
