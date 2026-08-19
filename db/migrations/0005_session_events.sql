CREATE TABLE `session_streams` (
	`stream_id` text PRIMARY KEY NOT NULL,
	`session_id` text NOT NULL,
	`parent_stream_id` text,
	`parent_seq` integer CHECK (`parent_seq` IS NULL OR `parent_seq` > 0),
	`generation` integer NOT NULL DEFAULT 0 CHECK (`generation` >= 0),
	`created_at_ms` integer NOT NULL,
	`retired_at_ms` integer,
	FOREIGN KEY (`parent_stream_id`) REFERENCES `session_streams`(`stream_id`)
);
--> statement-breakpoint
CREATE UNIQUE INDEX `session_streams_by_session_generation`
	ON `session_streams` (`session_id`, `generation`);
--> statement-breakpoint
CREATE TABLE `session_heads` (
	`stream_id` text PRIMARY KEY NOT NULL,
	`head_seq` integer NOT NULL DEFAULT 0 CHECK (`head_seq` >= 0),
	`revision` integer NOT NULL DEFAULT 0 CHECK (`revision` >= 0),
	`schema_version` integer NOT NULL CHECK (`schema_version` > 0),
	`last_event_id` text,
	`updated_at_ms` integer NOT NULL,
	FOREIGN KEY (`stream_id`) REFERENCES `session_streams`(`stream_id`) ON DELETE CASCADE
);
--> statement-breakpoint
CREATE TABLE `session_events` (
	`stream_id` text NOT NULL,
	`seq` integer NOT NULL CHECK (`seq` > 0),
	`event_id` text NOT NULL,
	`command_id` text,
	`schema_version` integer NOT NULL CHECK (`schema_version` > 0),
	`kind` text NOT NULL CHECK (length(`kind`) > 0),
	`payload_json` text NOT NULL CHECK (json_valid(`payload_json`)),
	`created_at_ms` integer NOT NULL,
	`runtime_id` text,
	`turn_id` text,
	PRIMARY KEY(`stream_id`, `seq`),
	FOREIGN KEY (`stream_id`) REFERENCES `session_streams`(`stream_id`) ON DELETE CASCADE
);
--> statement-breakpoint
CREATE UNIQUE INDEX `session_events_by_event_id`
	ON `session_events` (`stream_id`, `event_id`);
--> statement-breakpoint
CREATE INDEX `session_events_by_command`
	ON `session_events` (`stream_id`, `command_id`);
--> statement-breakpoint
CREATE INDEX `session_events_by_kind` ON `session_events` (`stream_id`,`kind`,`seq`);
--> statement-breakpoint
CREATE INDEX `session_events_by_turn` ON `session_events` (`stream_id`,`turn_id`,`seq`);
