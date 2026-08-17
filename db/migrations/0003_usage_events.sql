CREATE TABLE `usage_events` (
	`event_id` text PRIMARY KEY NOT NULL,
	`session_id` text NOT NULL,
	`project_path` text NOT NULL,
	`provider` text NOT NULL,
	`model` text NOT NULL,
	`timestamp_ms` integer NOT NULL,
	`input` integer NOT NULL,
	`output` integer NOT NULL,
	`cache_read` integer NOT NULL,
	`cache_write` integer NOT NULL,
	`reasoning` integer
);
--> statement-breakpoint
CREATE INDEX `usage_events_by_time` ON `usage_events` (`timestamp_ms`);
