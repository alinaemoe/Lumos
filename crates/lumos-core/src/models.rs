#[derive(Debug, Clone)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct ProjectPath {
    pub id: i64,
    pub project_id: i64,
    pub path: String,
    pub git_remote: Option<String>,
    pub repo_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size: i64,
    pub hash: Option<String>,
    pub file_type: Option<String>,
    pub project_id: Option<i64>,
    pub created_at: i64,
    pub modified_at: i64,
}
