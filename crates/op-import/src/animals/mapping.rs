//! Which column holds which animal field, guessed from the header the way
//! herd software writes it: "Tag", "Visual ID", "Ear tag"; "EID", "RFID",
//! "ISO"; "DOB", "Birth date"; "Sex", "Gender"; and so on.

use std::collections::BTreeMap;

/// The fields a file can fill, in guessing order (EID before tag, so "RFID
/// tag" is an EID).
pub const FIELDS: [&str; 8] = ["eid", "tag", "born", "sex", "breed", "collar", "name", "notes"];

/// field → column name.
pub type Mapping = BTreeMap<String, String>;

/// Header names that mean the field outright, lowercase letters and digits only.
fn exact(field: &str) -> &'static [&'static str] {
    match field {
        "eid" => &[
            "eid",
            "rfid",
            "iso",
            "electronicid",
            "eidnumber",
            "eidno",
            "eidtag",
            "rfidnumber",
            "rfidno",
            "rfidtag",
            "isonumber",
            "nlis",
            "electronictag",
            "eideartag",
        ],
        "tag" => &[
            "tag",
            "visualid",
            "vid",
            "eartag",
            "tagnumber",
            "tagno",
            "tagnum",
            "tagid",
            "visualtag",
            "animalid",
            "animaltag",
            "cowid",
            "cowtag",
            "id",
            "number",
            "no",
            "animal",
        ],
        "born" => &["dob", "born", "birthdate", "dateofbirth", "birthday", "birth", "bdate", "birthdt"],
        "sex" => &["sex", "gender"],
        "breed" => &["breed", "breeds"],
        "collar" => &["collar", "collarname", "collarid", "collarno"],
        "name" => &["name", "animalname", "cowname"],
        "notes" => &["notes", "note", "comments", "comment", "remarks", "remark"],
        _ => &[],
    }
}

/// Words that point to the field when no header matches outright.
fn partial(field: &str) -> &'static [&'static str] {
    match field {
        "eid" => &["eid", "rfid", "electronic", "nlis"],
        "tag" => &["tag", "visual"],
        "born" => &["birth", "dob", "born"],
        "sex" => &["sex", "gender"],
        "breed" => &["breed"],
        "collar" => &["collar"],
        "name" => &["name"],
        "notes" => &["note", "comment", "remark"],
        _ => &[],
    }
}

/// "Ear Tag #" → "eartag".
pub fn norm(header: &str) -> String {
    header.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// The best guess for each field that has a column. Each column fills at most one field.
pub fn guess(columns: &[String]) -> Mapping {
    let normed: Vec<String> = columns.iter().map(|c| norm(c)).collect();
    let mut used = vec![false; columns.len()];
    let mut out = Mapping::new();
    for pass in 0..2 {
        for field in FIELDS {
            if out.contains_key(field) {
                continue;
            }
            let hit = (0..columns.len()).find(|&i| {
                !used[i] && if pass == 0 { exact(field).contains(&normed[i].as_str()) } else { partial(field).iter().any(|w| normed[i].contains(w)) }
            });
            if let Some(i) = hit {
                used[i] = true;
                out.insert(field.to_owned(), columns[i].clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(cols: &[&str]) -> Vec<(String, String)> {
        guess(&cols.iter().map(|s| s.to_string()).collect::<Vec<_>>()).into_iter().collect()
    }

    fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = p.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        v.sort();
        v
    }

    #[test]
    fn common_exports() {
        assert_eq!(
            g(&["Visual ID", "EID", "Name", "Breed", "Sex", "DOB"]),
            pairs(&[("tag", "Visual ID"), ("eid", "EID"), ("name", "Name"), ("breed", "Breed"), ("sex", "Sex"), ("born", "DOB")])
        );
        assert_eq!(
            g(&["Ear Tag #", "RFID Tag", "Gender", "Birth Date", "Collar"]),
            pairs(&[("tag", "Ear Tag #"), ("eid", "RFID Tag"), ("sex", "Gender"), ("born", "Birth Date"), ("collar", "Collar")])
        );
        assert_eq!(g(&["Tag", "ISO", "Born", "Comments"]), pairs(&[("tag", "Tag"), ("eid", "ISO"), ("born", "Born"), ("notes", "Comments")]));
    }

    #[test]
    fn near_names_fall_back_to_words() {
        assert_eq!(
            g(&["Cow Tag Number", "Date of Birth (mm/dd/yyyy)", "Animal Name"]),
            pairs(&[("tag", "Cow Tag Number"), ("born", "Date of Birth (mm/dd/yyyy)"), ("name", "Animal Name")])
        );
    }

    #[test]
    fn unknown_columns_stay_unmapped() {
        assert_eq!(g(&["Weight", "Pen"]), vec![]);
    }
}
