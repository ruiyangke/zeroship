include!("../../../tests/fixtures/platform_db/overlay.rs");

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str =
        "[control]\ndatabase_url = \"postgres://postgres:zeroship@127.0.0.1:5440/zeroship\"\n";

    #[test]
    fn reads_a_key_from_the_named_section() {
        assert_eq!(
            get(DOC, "control", "database_url").as_deref(),
            Some("postgres://postgres:zeroship@127.0.0.1:5440/zeroship")
        );
    }

    #[test]
    fn a_key_in_another_section_is_not_found() {
        assert_eq!(get(DOC, "worker", "database_url"), None);
    }

    #[test]
    fn comments_and_absent_keys_return_nothing() {
        assert_eq!(
            get(
                "# [control]\n# database_url = \"x\"\n",
                "control",
                "database_url"
            ),
            None
        );
        assert_eq!(get(DOC, "control", "nope"), None);
    }

    #[test]
    fn splits_a_plain_dsn() {
        let p = split_dsn("postgres://postgres:zeroship@127.0.0.1:5440/zeroship");
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, "5440");
        assert_eq!(p.user, "postgres");
        assert_eq!(p.pass, "zeroship");
        assert_eq!(p.db, "zeroship");
    }

    #[test]
    fn an_at_sign_in_the_password_does_not_truncate_the_host() {
        let p = split_dsn("postgres://u:p@ss@db.example.com:5432/x");
        assert_eq!(p.host, "db.example.com");
        assert_eq!(p.pass, "p@ss");
    }

    #[test]
    fn a_missing_port_defaults_and_a_missing_password_is_empty() {
        let p = split_dsn("postgres://someone@host/db");
        assert_eq!(p.port, "5432");
        assert_eq!(p.user, "someone");
        assert_eq!(p.pass, "");
    }

    #[test]
    fn query_parameters_are_not_part_of_the_database_name() {
        let p = split_dsn("postgres://u:p@h:5432/db?sslmode=disable");
        assert_eq!(p.db, "db");
    }

    #[test]
    fn the_wanted_block_is_read_from_its_own_channel() {
        let w = Wanted::from_block("host=127.0.0.1\nport=5440\nuser=postgres\npass=p=q\nnoise\n");
        assert_eq!(w.host, "127.0.0.1");
        assert_eq!(w.port, "5440");
        assert_eq!(w.user, "postgres");
        // Split on the FIRST `=`, so a password containing one survives.
        assert_eq!(w.pass, "p=q");
        assert_eq!(Wanted::from_block(""), Wanted::default());
    }

    #[test]
    fn loopback_spellings_agree_but_real_hosts_do_not() {
        assert!(same_host("localhost", "127.0.0.1"));
        assert!(same_host("::1", "localhost"));
        assert!(!same_host("db.example.com", "127.0.0.1"));
    }
}
