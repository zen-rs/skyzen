//! Deep-merging of TOML tables.

/// Deep-merge `overlay` into `base`.
///
/// The rule is deliberately simple, because it is the *documented* contract for two different
/// user-facing features — `[cloudflare.env.<name>]` overlays and the `[cloudflare.raw]` escape
/// hatch — and both need to be predictable from reading the manifest alone:
///
/// * When a key holds a **table** on both sides, the tables are merged key by key, recursively.
/// * Otherwise the overlay's value **replaces** the base's. Arrays are values, so an overlay
///   array replaces the base array wholesale rather than appending to it — there is no
///   element identity to merge on, and a half-merged array of bindings is worse than a
///   replaced one.
/// * A key present only in the base survives untouched.
///
/// # Examples
///
/// ```
/// # use skyzen_manifest::deep_merge;
/// let mut base: toml::Table = toml::from_str("a = 1\n[t]\nx = 1\ny = 1\n").unwrap();
/// let overlay: toml::Table = toml::from_str("[t]\ny = 2\nz = 3\n").unwrap();
/// deep_merge(&mut base, overlay);
///
/// assert_eq!(base["a"].as_integer(), Some(1));
/// let table = base["t"].as_table().unwrap();
/// assert_eq!(table["x"].as_integer(), Some(1));
/// assert_eq!(table["y"].as_integer(), Some(2));
/// assert_eq!(table["z"].as_integer(), Some(3));
/// ```
pub fn deep_merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, overlay_value) in overlay {
        match (base.get_mut(&key), overlay_value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
                deep_merge(base_table, overlay_table);
            }
            (_, overlay_value) => {
                base.insert(key, overlay_value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::deep_merge;

    fn table(source: &str) -> toml::Table {
        toml::from_str(source).expect("valid TOML")
    }

    #[test]
    fn nested_tables_merge_key_by_key() {
        let mut base = table("[a.b]\nkeep = 1\nreplace = 1\n");
        deep_merge(&mut base, table("[a.b]\nreplace = 2\nadd = 3\n"));

        let inner = base["a"]["b"].as_table().expect("nested table");
        assert_eq!(inner["keep"].as_integer(), Some(1));
        assert_eq!(inner["replace"].as_integer(), Some(2));
        assert_eq!(inner["add"].as_integer(), Some(3));
    }

    #[test]
    fn arrays_are_replaced_not_appended() {
        let mut base = table("items = [1, 2, 3]\n");
        deep_merge(&mut base, table("items = [9]\n"));
        assert_eq!(base["items"].as_array().expect("array").len(), 1);
    }

    #[test]
    fn a_scalar_replaces_a_table_and_a_table_replaces_a_scalar() {
        let mut base = table("[value]\nnested = 1\n");
        deep_merge(&mut base, table("value = 7\n"));
        assert_eq!(base["value"].as_integer(), Some(7));

        let mut base = table("value = 7\n");
        deep_merge(&mut base, table("[value]\nnested = 1\n"));
        assert_eq!(base["value"]["nested"].as_integer(), Some(1));
    }

    #[test]
    fn base_only_keys_survive() {
        let mut base = table("kept = true\n");
        deep_merge(&mut base, table("added = true\n"));
        assert_eq!(base["kept"].as_bool(), Some(true));
        assert_eq!(base["added"].as_bool(), Some(true));
    }
}
