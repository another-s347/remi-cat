use crate::TriggerConfig;

/// Test scenarios based on real-world event patterns from events.json
/// These tests validate rule-based triggers against realistic lifetime scenarios

#[cfg(test)]
mod realworld_scenario_tests {
    use super::*;

    /// Scenario 1: Connectivity Loss Alert
    /// Pattern observed: Device shows "none" connectivity states with no wifi/mobile recovery
    #[test]
    fn test_connectivity_loss_alert_config() {
        let json = r#"{
            "name": "Connectivity Loss Alert",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('*/10 * * * *')",
                    "description": "Check every 10 minutes"
                }
            ],
            "condition": [
                {
                    "rule": "event_count(30, 'Connectivity') > 0",
                    "description": "Recent connectivity events exist"
                },
                {
                    "rule": "event_exists_with_message(30, 'Connectivity', 'none')",
                    "description": "Device lost connectivity"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(
            config.is_ok(),
            "Failed to parse connectivity loss alert config: {:?}",
            config
        );

        let config = config.unwrap();
        assert_eq!(config.name, "Connectivity Loss Alert");
        assert_eq!(config.precondition.len(), 1);
        assert_eq!(config.condition.len(), 2);

        // Verify cron expression in precondition
        assert!(config.precondition[0].rule.contains("cron"));
        assert!(config.precondition[0].rule.contains("*/10"));
    }

    /// Scenario 2: Location Change Detection
    #[test]
    fn test_location_change_detector_config() {
        let json = r#"{
            "name": "Location Change Detector",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('*/5 * * * *')",
                    "description": "Check every 5 minutes"
                }
            ],
            "condition": [
                {
                    "rule": "event_exists(10, 'Location')",
                    "description": "Location events exist"
                },
                {
                    "rule": "event_exists_with_message(10, 'Location', 'Movement position update')",
                    "description": "Movement detected"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert_eq!(config.name, "Location Change Detector");
        assert_eq!(config.condition.len(), 2);

        // Verify conditions reference Location events
        assert!(config.condition[0].rule.contains("Location"));
        assert!(config.condition[1].rule.contains("Movement"));
    }

    /// Scenario 3: Idle Device During Work Hours
    #[test]
    fn test_idle_device_work_hours_config() {
        let json = r#"{
            "name": "Work Hours Idle Detection",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('0 */1 * * *')",
                    "description": "Check every hour"
                }
            ],
            "condition": [
                {
                    "rule": "in_time_range('09:00', '17:00')",
                    "description": "During work hours"
                },
                {
                    "rule": "is_weekday(['mon', 'tue', 'wed', 'thu', 'fri'])",
                    "description": "Only on weekdays"
                },
                {
                    "rule": "event_count(120, 'Location') == 0",
                    "description": "No location events in 2 hours"
                },
                {
                    "rule": "event_count(120, 'System') > 0",
                    "description": "System is active"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert_eq!(config.name, "Work Hours Idle Detection");
        assert_eq!(config.condition.len(), 4);

        // Verify time and weekday checks
        assert!(config.condition[0].rule.contains("in_time_range"));
        assert!(config.condition[1].rule.contains("is_weekday"));
    }

    /// Scenario 4: Connectivity Instability Alert
    #[test]
    fn test_connectivity_instability_config() {
        let json = r#"{
            "name": "Connectivity Instability Alert",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('*/15 * * * *')",
                    "description": "Check every 15 minutes"
                }
            ],
            "condition": [
                {
                    "rule": "event_count(15, 'Connectivity') > 10",
                    "description": "More than 10 events indicates instability"
                },
                {
                    "rule": "event_exists_with_message(15, 'Connectivity', 'none')",
                    "description": "Connection drops detected"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert!(config.condition[0].rule.contains("event_count"));
        assert!(config.condition[0].rule.contains("> 10"));
    }

    /// Scenario 5: Evening Location Tracking Reminder
    #[test]
    fn test_evening_location_reminder_config() {
        let json = r#"{
            "name": "Evening Location Pause Reminder",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('0 22 * * *')",
                    "description": "Every evening at 10 PM"
                }
            ],
            "condition": [
                {
                    "rule": "event_count(60, 'Location') > 0",
                    "description": "Location tracking is active"
                },
                {
                    "rule": "current_hour() >= 22 || current_hour() < 7",
                    "description": "Late evening or early morning"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert!(config.precondition[0].rule.contains("22"));
        assert!(config.condition[1].rule.contains("current_hour"));
    }

    /// Scenario 6: Weekly Activity Summary
    #[test]
    fn test_weekly_activity_summary_config() {
        let json = r#"{
            "name": "Weekly Activity Summary",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('0 9 * * 0')",
                    "description": "Every Sunday at 9 AM"
                }
            ],
            "condition": [
                {
                    "rule": "event_count(10080, 'Location') > 0",
                    "description": "Check past week (10080 minutes)"
                },
                {
                    "rule": "is_weekday(['sun'])",
                    "description": "Ensure it's Sunday"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert!(config.precondition[0].rule.contains("0 9 * * 0"));
        assert!(config.condition[0].rule.contains("10080"));
    }

    /// Scenario 7: VPN Usage Detection
    #[test]
    fn test_vpn_detection_config() {
        let json = r#"{
            "name": "VPN Usage Alert",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('*/30 * * * *')",
                    "description": "Check every 30 minutes"
                }
            ],
            "condition": [
                {
                    "rule": "event_exists_with_message(60, 'Connectivity', 'vpn')",
                    "description": "VPN connection detected"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert!(config.condition[0].rule.contains("vpn"));
    }

    /// Scenario 8: System Startup Detection
    #[test]
    fn test_system_startup_detection_config() {
        let json = r#"{
            "name": "System Startup Monitor",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('*/1 * * * *')",
                    "description": "Check every minute"
                }
            ],
            "condition": [
                {
                    "rule": "event_exists_with_message(5, 'System', 'Starting monitors')",
                    "description": "System startup detected"
                },
                {
                    "rule": "event_exists_with_message(5, 'System', 'Rust bridge active')",
                    "description": "Rust bridge initialized"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert!(config.condition[0].rule.contains("Starting monitors"));
        assert!(config.condition[1].rule.contains("Rust bridge active"));
    }

    /// Scenario 10: Complex Multi-Condition Trigger
    #[test]
    fn test_complex_multi_condition_config() {
        let json = r#"{
            "name": "Outdoor Movement with Unstable Connection",
            "version": "v1",
            "precondition": [
                {
                    "rule": "cron('*/10 * * * *')",
                    "description": "Check every 10 minutes"
                }
            ],
            "condition": [
                {
                    "rule": "event_count(30, 'Location') > 2",
                    "description": "Multiple location updates (moving)"
                },
                {
                    "rule": "event_exists_with_message(30, 'Connectivity', 'mobile')",
                    "description": "On mobile data"
                },
                {
                    "rule": "event_count(30, 'Connectivity') > 5",
                    "description": "Frequent connectivity changes"
                },
                {
                    "rule": "!event_exists_with_message(30, 'Connectivity', 'wifi')",
                    "description": "Not on stable wifi"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert_eq!(config.condition.len(), 4);

        // Verify complex logic with negation
        assert!(config.condition[3].rule.starts_with("!"));
    }
}
