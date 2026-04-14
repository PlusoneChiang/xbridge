use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct DetectableApp {
    pub id: String,
    pub name: String,
    pub executables: Option<Vec<AppExecutable>>,
}

#[derive(Debug, Deserialize)]
pub struct AppExecutable {
    pub name: String,
    #[serde(default)]
    pub is_launcher: bool,
}
