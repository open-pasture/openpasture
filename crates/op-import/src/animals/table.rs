//! A spreadsheet export read into named columns and rows of text. Herd
//! records come out of Excel, CattleMax, Herdwatch and the like: comma,
//! semicolon or tab separated, UTF-8 (with or without a byte order mark),
//! UTF-16 ("Unicode text") or Windows-1252.

/// Header cells (unique, never empty) and the rows under them, every row as
/// wide as the header. Rows with nothing in them are left out.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Most rows one file may hold.
pub const MAX_ROWS: usize = 50_000;

impl Table {
    /// The column's index by name.
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }
}

/// Read a delimited text file whose first non-empty row is the header.
pub fn read(bytes: &[u8]) -> Result<Table, String> {
    let text = decode(bytes);
    let delim = delimiter(&text);
    let mut rdr = csv::ReaderBuilder::new().delimiter(delim).has_headers(false).flexible(true).trim(csv::Trim::All).from_reader(text.as_bytes());
    let mut header: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| format!("The file isn't readable as CSV: {e}."))?;
        let cells: Vec<String> = rec.iter().map(|c| c.trim().to_owned()).collect();
        if cells.iter().all(|c| c.is_empty()) {
            continue;
        }
        if header.is_none() {
            header = Some(cells);
            continue;
        }
        if rows.len() >= MAX_ROWS {
            return Err(format!("The file has more than {MAX_ROWS} rows."));
        }
        rows.push(cells);
    }
    let Some(header) = header else {
        return Err("The file is empty.".into());
    };
    let width = rows.iter().map(Vec::len).chain([header.len()]).max().unwrap_or(0);
    let mut columns: Vec<String> = Vec::with_capacity(width);
    for i in 0..width {
        let base = header.get(i).map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(|| format!("Column {}", i + 1));
        let mut name = base.clone();
        let mut n = 2;
        while columns.contains(&name) {
            name = format!("{base} ({n})");
            n += 1;
        }
        columns.push(name);
    }
    for r in &mut rows {
        r.resize(width, String::new());
    }
    Ok(Table { columns, rows })
}

/// Text from bytes: UTF-8 (BOM dropped), UTF-16 with a BOM, else Windows-1252.
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let le = bytes[0] == 0xFF;
        let units: Vec<u16> =
            bytes[2..].chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        return String::from_utf16_lossy(&units);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    }
}

/// Windows-1252, the encoding of a CSV saved by Excel on Windows.
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™',
        'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

/// The separator the header row uses most, outside quotes: comma, semicolon
/// (European Excel) or tab.
fn delimiter(text: &str) -> u8 {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut quoted = false;
    let mut counts = [0usize; 3];
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => counts[0] += 1,
            ';' if !quoted => counts[1] += 1,
            '\t' if !quoted => counts[2] += 1,
            _ => {}
        }
    }
    let best = (0..3).max_by_key(|&i| (counts[i], std::cmp::Reverse(i))).unwrap_or(0);
    if counts[best] == 0 { b',' } else { [b',', b';', b'\t'][best] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commas_semicolons_and_tabs() {
        let t = read(b"Tag,Name\n214,Daisy\n\n215,\n").unwrap();
        assert_eq!(t.columns, ["Tag", "Name"]);
        assert_eq!(t.rows, [vec!["214", "Daisy"], vec!["215", ""]]);
        let t = read("\u{feff}Tag;Naam\r\n214;Daisy\r\n".as_bytes()).unwrap();
        assert_eq!(t.columns, ["Tag", "Naam"]);
        assert_eq!(t.rows, [vec!["214", "Daisy"]]);
        let t = read(b"Tag\tDOB\n214\t4/1/2022\n").unwrap();
        assert_eq!(t.rows, [vec!["214", "4/1/2022"]]);
    }

    #[test]
    fn quoted_cells_keep_their_commas() {
        let t = read(b"Tag,Notes\n214,\"calm, easy to move\"\n").unwrap();
        assert_eq!(t.rows[0][1], "calm, easy to move");
    }

    #[test]
    fn blank_and_repeated_headers_get_names_and_short_rows_are_padded() {
        let t = read(b"Tag,,Tag\n1,2,3,4\n5\n").unwrap();
        assert_eq!(t.columns, ["Tag", "Column 2", "Tag (2)", "Column 4"]);
        assert_eq!(t.rows[1], ["5", "", "", ""]);
    }

    #[test]
    fn utf16_and_windows_1252() {
        let mut b = vec![0xFF, 0xFE];
        for u in "Tag\tName\n214\tZoë\n".encode_utf16() {
            b.extend(u.to_le_bytes());
        }
        let t = read(&b).unwrap();
        assert_eq!(t.rows[0], ["214", "Zoë"]);
        let t = read(b"Tag,Name\n214,Zo\xEB \x96 red\n").unwrap();
        assert_eq!(t.rows[0][1], "Zoë – red");
    }

    #[test]
    fn empty_files_say_so() {
        assert!(read(b"").is_err());
        assert!(read(b"\n,,\n").is_err());
    }
}
