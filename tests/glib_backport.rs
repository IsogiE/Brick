#![cfg(target_os = "linux")]

use gtk::glib::variant::ToVariant;

#[test]
fn optimized_glib_string_iterator_preserves_front_and_back_values() {
    let values = ["first", "", "unicode: λ", "last"];
    let variant = values.to_variant();
    assert_eq!(
        variant.array_iter_str().unwrap().collect::<Vec<_>>(),
        values
    );
    let mut iter = variant.array_iter_str().unwrap();
    assert_eq!(iter.next(), Some("first"));
    assert_eq!(iter.next_back(), Some("last"));
    assert_eq!(iter.nth(1), Some("unicode: λ"));
    assert_eq!(iter.next(), None);
    assert_eq!(
        variant.array_iter_str().unwrap().nth_back(1),
        Some("unicode: λ")
    );
    assert_eq!(variant.array_iter_str().unwrap().last(), Some("last"));
}
