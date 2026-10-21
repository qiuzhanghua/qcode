use std::collections::BTreeSet;

use qcode_gui::forms::{FIELDS, GUI_FIELDS, Page, TABLES};

#[test]
fn every_known_config_key_has_a_control() {
    let known = qcode_kernel::config::known_keys();
    let sections: BTreeSet<&str> = known.iter().map(|(section, _)| *section).collect();
    let mut missing = Vec::new();
    for (section, keys) in known {
        for key in *keys {
            if *key == "version" || (section.is_empty() && sections.contains(key)) {
                continue;
            }
            let path: Vec<&str> = if section.is_empty() {
                vec![key]
            } else {
                vec![section, key]
            };
            let scalar = FIELDS.iter().any(|field| field.path == path);
            let table = TABLES.iter().any(|table| table.path == path);
            if !scalar && !table {
                missing.push(path.join("."));
            }
        }
    }
    assert!(missing.is_empty(), "missing form controls: {missing:?}");
}

#[test]
fn every_page_has_a_title_and_some_control() {
    for page in Page::ALL {
        assert!(!page.title().is_empty());
    }
    for page in Page::ALL
        .into_iter()
        .filter(|page| !matches!(page, Page::Welcome | Page::Review))
    {
        let fields = FIELDS.iter().filter(|field| field.page == page).count();
        let tables = TABLES.iter().filter(|table| table.page == page).count();
        let gui = GUI_FIELDS.iter().filter(|field| field.page == page).count();
        assert!(fields + tables + gui > 0, "{page:?} has no controls");
    }
}

#[test]
fn paths_and_labels_are_unique() {
    let mut paths = std::collections::BTreeSet::new();
    for field in FIELDS.iter().chain(GUI_FIELDS) {
        assert!(paths.insert(field.path), "duplicate field {:?}", field.path);
        assert!(!field.label.is_empty());
    }
    let mut tables = std::collections::BTreeSet::new();
    for table in TABLES {
        assert!(
            tables.insert(table.path),
            "duplicate table {:?}",
            table.path
        );
        assert!(!table.columns.is_empty());
    }
}
