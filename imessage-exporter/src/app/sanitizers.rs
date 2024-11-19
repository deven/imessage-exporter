use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use std::borrow::Cow;

/// Characters disallowed in a filename
static FILENAME_DISALLOWED_CHARS: LazyLock<HashSet<&char>> = LazyLock::new(|| {
    let mut set = HashSet::new();
    set.insert(&'*');
    set.insert(&'"');
    set.insert(&'/');
    set.insert(&'\\');
    set.insert(&'<');
    set.insert(&'>');
    set.insert(&':');
    set.insert(&'|');
    set.insert(&'?');
    set
});

/// Characters disallowed in HTML
static HTML_DISALLOWED_CHARS: LazyLock<HashMap<&char, &str>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert(&'>', "&gt;");
    map.insert(&'<', "&lt;");
    map.insert(&'"', "&quot;");
    map.insert(&'\'', "&apos;");
    map.insert(&'`', "&grave;");
    map.insert(&'&', "&amp;");
    map.insert(&' ', "&nbsp;");
    map
});

/// Characters disallowed in JSON strings
static JSON_DISALLOWED_CHARS: LazyLock<HashMap<&char, &str>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert(&'"', "\\\"");
    map.insert(&'\\', "\\\\");
    map.insert(&'\x00', "\\u0000");
    map.insert(&'\x01', "\\u0001");
    map.insert(&'\x02', "\\u0002");
    map.insert(&'\x03', "\\u0003");
    map.insert(&'\x04', "\\u0004");
    map.insert(&'\x05', "\\u0005");
    map.insert(&'\x06', "\\u0006");
    map.insert(&'\x07', "\\u0007");
    map.insert(&'\x08', "\\b");
    map.insert(&'\x09', "\\t");
    map.insert(&'\x0a', "\\n");
    map.insert(&'\x0b', "\\u000b");
    map.insert(&'\x0c', "\\f");
    map.insert(&'\x0d', "\\r");
    map.insert(&'\x0e', "\\u000e");
    map.insert(&'\x0f', "\\u000f");
    map.insert(&'\x10', "\\u0010");
    map.insert(&'\x11', "\\u0011");
    map.insert(&'\x12', "\\u0012");
    map.insert(&'\x13', "\\u0013");
    map.insert(&'\x14', "\\u0014");
    map.insert(&'\x15', "\\u0015");
    map.insert(&'\x16', "\\u0016");
    map.insert(&'\x17', "\\u0017");
    map.insert(&'\x18', "\\u0018");
    map.insert(&'\x19', "\\u0019");
    map.insert(&'\x1a', "\\u001a");
    map.insert(&'\x1b', "\\u001b");
    map.insert(&'\x1c', "\\u001c");
    map.insert(&'\x1d', "\\u001d");
    map.insert(&'\x1e', "\\u001e");
    map.insert(&'\x1f', "\\u001f");
    map
});

/// The character to replace disallowed chars with
const FILENAME_REPLACEMENT_CHAR: char = '_';

/// Remove unsafe chars in [this list](FILENAME_DISALLOWED_CHARS).
pub fn sanitize_filename(filename: &str) -> String {
    filename
        .chars()
        .map(|letter| {
            if FILENAME_DISALLOWED_CHARS.contains(&letter) {
                FILENAME_REPLACEMENT_CHAR
            } else {
                letter
            }
        })
        .collect()
}

/// Escapes HTML special characters in the input string.
pub fn sanitize_html(input: &str) -> Cow<str> {
    for (idx, c) in input.char_indices() {
        if HTML_DISALLOWED_CHARS.contains_key(&c) {
            let mut res = String::from(&input[..idx]);
            input[idx..]
                .chars()
                .for_each(|c| match HTML_DISALLOWED_CHARS.get(&c) {
                    Some(replacement) => res.push_str(replacement),
                    None => res.push(c),
                });
            return Cow::Owned(res);
        }
    }
    Cow::Borrowed(input)
}

/// Escapes JSON special characters and control characters in the input string.
pub fn sanitize_json(input: &str) -> Cow<str> {
    for (idx, c) in input.char_indices() {
        if JSON_DISALLOWED_CHARS.contains_key(&c) {
            let mut res = String::from(&input[..idx]);
            input[idx..]
                .chars()
                .for_each(|c| match JSON_DISALLOWED_CHARS.get(&c) {
                    Some(replacement) => res.push_str(replacement),
                    None => res.push(c),
                });
            return Cow::Owned(res);
        }
    }
    Cow::Borrowed(input)
}

#[cfg(test)]
mod test_filename {
    use crate::app::sanitizers::sanitize_filename;

    #[test]
    fn can_sanitize_macos() {
        assert_eq!(sanitize_filename("a/b\\c:d"), "a_b_c_d");
    }

    #[test]
    fn doesnt_sanitize_none() {
        assert_eq!(sanitize_filename("a_b_c_d"), "a_b_c_d");
    }

    #[test]
    fn can_sanitize_one() {
        assert_eq!(sanitize_filename("ab/cd"), "ab_cd");
    }

    #[test]
    fn can_sanitize_only_bad() {
        assert_eq!(
            sanitize_filename("* \" / \\ < > : | ?"),
            "_ _ _ _ _ _ _ _ _"
        );
    }
}

#[cfg(test)]
mod test_html {
    use crate::app::sanitizers::sanitize_html;

    #[test]
    fn test_escape_html_chars_basic() {
        assert_eq!(
            &sanitize_html("<p>Hello, world > HTML</p>"),
            "&lt;p&gt;Hello, world &gt; HTML&lt;/p&gt;"
        );
    }

    #[test]
    fn doesnt_sanitize_empty_string() {
        assert_eq!(&sanitize_html(""), "");
    }

    #[test]
    fn doesnt_sanitize_no_special_chars() {
        assert_eq!(&sanitize_html("Hello world"), "Hello world");
    }

    #[test]
    fn can_sanitize_code_block() {
        assert_eq!(
            &sanitize_html("`imessage-exporter -f txt`"),
            "&grave;imessage-exporter -f txt&grave;"
        );
    }

    #[test]
    fn can_sanitize_all_special_chars() {
        assert_eq!(
            &sanitize_html("<>&\"`'"),
            "&lt;&gt;&amp;&quot;&grave;&apos;"
        );
    }

    #[test]
    fn can_sanitize_mixed_content() {
        assert_eq!(
            &sanitize_html("<div>Hello &amp; world</div>"),
            "&lt;div&gt;Hello &amp;amp; world&lt;/div&gt;"
        );
    }

    #[test]
    fn can_sanitize_mixed_content_nbsp() {
        assert_eq!(
            &sanitize_html("<div>Hello &amp; world</div>"),
            "&lt;div&gt;Hello&nbsp;&amp;amp;&nbsp;world&lt;/div&gt;"
        );
    }
}

#[cfg(test)]
mod test_json {
    use crate::app::sanitizers::sanitize_json;

    #[test]
    fn test_escape_json_chars_basic() {
        assert_eq!(
            &sanitize_json("Hello \"world\" \\ JSON"),
            "Hello \\\"world\\\" \\\\ JSON"
        );
    }

    #[test]
    fn doesnt_sanitize_empty_string() {
        assert_eq!(&sanitize_json(""), "");
    }

    #[test]
    fn doesnt_sanitize_no_special_chars() {
        assert_eq!(&sanitize_json("Hello world"), "Hello world");
    }

    #[test]
    fn can_escape_control_characters() {
        assert_eq!(
            &sanitize_json("Line1\nLine2\tTabbed"),
            "Line1\\nLine2\\tTabbed"
        );
    }

    #[test]
    fn can_escape_all_control_characters() {
        assert_eq!(
            &sanitize_json(concat!(
                "\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
                "\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f"
            )),
            concat!(
                "\\u0000\\u0001\\u0002\\u0003\\u0004\\u0005\\u0006\\u0007\\b\\t\\n",
                "\\u000b\\f\\r\\u000e\\u000f\\u0010\\u0011\\u0012\\u0013\\u0014\\u0015",
                "\\u0016\\u0017\\u0018\\u0019\\u001a\\u001b\\u001c\\u001d\\u001e\\u001f"
            )
        );
    }

    #[test]
    fn can_escape_mixed_content() {
        assert_eq!(
            &sanitize_json("Key: \"value\" with \\ control and \n newline"),
            "Key: \\\"value\\\" with \\\\ control and \\n newline"
        );
    }

    #[test]
    fn can_escape_special_json_characters() {
        assert_eq!(
            &sanitize_json("\"\\/\x08\x0c\n\r\t"),
            "\\\"\\\\/\\b\\f\\n\\r\\t"
        );
    }

    #[test]
    fn sanitizes_complex_content_with_control_chars() {
        assert_eq!(
            &sanitize_json("Complex: \"Line1\nLine2\\Tab\" with control chars \x1f"),
            "Complex: \\\"Line1\\nLine2\\\\Tab\\\" with control chars \\u001f"
        );
    }
}
