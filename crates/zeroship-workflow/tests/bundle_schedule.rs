use zeroship_workflow::service::ScheduleRegistration;

#[test]
fn manifest_schedules_match_the_compiler_wire_contract() {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/bundle-schedules.json")).unwrap();
    assert!(!fixtures.is_empty());
    for fixture in fixtures {
        let schedule: ScheduleRegistration = serde_json::from_value(fixture.clone()).unwrap();
        assert!(schedule.schedule.next_after(0, 0).unwrap() > 0);
        assert_eq!(serde_json::to_value(schedule).unwrap(), fixture);
        for policy in ["overlap", "catchUp"] {
            let mut duplicate = fixture.clone();
            duplicate["schedule"][policy] = fixture[policy].clone();
            assert!(serde_json::from_value::<ScheduleRegistration>(duplicate).is_err());
        }
    }
}
