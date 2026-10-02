-- Newsletter eligibility is based on consent, not campaign size or a daily budget.
ALTER TABLE mail_settings DROP COLUMN max_daily_confirmation_messages;
ALTER TABLE mail_settings DROP COLUMN max_daily_messages;
ALTER TABLE mail_settings DROP COLUMN max_campaign_recipients;
ALTER TABLE mail_settings DROP COLUMN send_interval_milliseconds;
ALTER TABLE mail_control_state DROP COLUMN max_daily_confirmations;
ALTER TABLE mail_control_state DROP COLUMN max_daily_messages;
ALTER TABLE mail_control_state DROP COLUMN max_campaign_recipients;
