use std::path::Path;

/// The file formats Data Analyzer can open, detected from the file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Csv,
    Tsv,
    Parquet,
    Json,
    NdJson,
    Excel,
}

impl FileKind {
    /// Every extension we accept, for open dialogs and drag-and-drop filters.
    pub const EXTENSIONS: &[&str] = &[
        "csv", "tsv", "tab", "txt", "psv", "parquet", "pq", "json", "ndjson", "jsonl", "xlsx",
        "xlsm", "xlsb", "xls", "ods", "gz", "zst",
    ];

    /// Detects the kind from the extension, looking through `.gz`/`.zst`.
    pub fn detect(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        let name = name
            .strip_suffix(".gz")
            .or_else(|| name.strip_suffix(".zst"))
            .unwrap_or(&name);
        Some(match name.rsplit_once('.')?.1 {
            "csv" | "txt" | "psv" | "dat" => Self::Csv,
            "tsv" | "tab" => Self::Tsv,
            "parquet" | "pq" => Self::Parquet,
            "json" => Self::Json,
            "ndjson" | "jsonl" => Self::NdJson,
            "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => Self::Excel,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Parquet => "Parquet",
            Self::Json => "JSON",
            Self::NdJson => "NDJSON",
            Self::Excel => "Excel",
        }
    }

    /// Whether opening shows a streamed preview first and loads the full file in
    /// the background. Parquet is fast to query in place, and Excel is read
    /// whole by calamine, so neither needs it.
    pub fn needs_materialize(self) -> bool {
        matches!(self, Self::Csv | Self::Tsv | Self::Json | Self::NdJson)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension() {
        let cases = [
            ("a.csv", Some(FileKind::Csv)),
            ("A.CSV", Some(FileKind::Csv)),
            ("a.csv.gz", Some(FileKind::Csv)),
            ("a.tsv", Some(FileKind::Tsv)),
            ("a.parquet", Some(FileKind::Parquet)),
            ("a.jsonl", Some(FileKind::NdJson)),
            ("a.json", Some(FileKind::Json)),
            ("a.xls", Some(FileKind::Excel)),
            ("a.xlsx", Some(FileKind::Excel)),
            ("a.png", None),
            ("noext", None),
        ];
        for (name, want) in cases {
            assert_eq!(FileKind::detect(Path::new(name)), want, "{name}");
        }
    }
}
