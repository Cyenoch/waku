CREATE TABLE `trajectory_sessions` (
	`session_id` text PRIMARY KEY NOT NULL,
	`schema_version` integer NOT NULL,
	`generation` integer NOT NULL,
	`revision` integer NOT NULL,
	`next_sequence` integer NOT NULL,
	`availability` text NOT NULL,
	FOREIGN KEY (`session_id`) REFERENCES `sessions`(`id`) ON UPDATE no action ON DELETE cascade
);
--> statement-breakpoint
CREATE TABLE `trajectory_prompt_snapshots` (
	`session_id` text NOT NULL,
	`prompt_id` text NOT NULL,
	`sequence` integer NOT NULL,
	`fingerprint` text NOT NULL,
	`system_prompt` text,
	`tools_json` text NOT NULL,
	`options_json` text NOT NULL,
	`model_hint` text NOT NULL,
	`created_at_ms` integer NOT NULL,
	PRIMARY KEY(`session_id`, `prompt_id`),
	FOREIGN KEY (`session_id`) REFERENCES `trajectory_sessions`(`session_id`) ON UPDATE no action ON DELETE cascade
);
--> statement-breakpoint
CREATE INDEX `trajectory_prompts_by_sequence` ON `trajectory_prompt_snapshots` (`session_id`,`sequence`);
--> statement-breakpoint
CREATE TABLE `trajectory_records` (
	`session_id` text NOT NULL,
	`record_id` text NOT NULL,
	`sequence` integer NOT NULL,
	`revision` integer NOT NULL,
	`request_id` text,
	`parent_record_id` text,
	`prompt_id` text,
	`turn_count` integer NOT NULL,
	`step` integer NOT NULL,
	`kind` text NOT NULL,
	`lane` text NOT NULL,
	`status` text NOT NULL,
	`title` text NOT NULL,
	`preview` text NOT NULL,
	`search_text` text NOT NULL,
	`started_at_ms` integer,
	`first_token_at_ms` integer,
	`completed_at_ms` integer,
	`duration_ms` integer,
	`ttft_ms` integer,
	`detail_json` text NOT NULL,
	PRIMARY KEY(`session_id`, `record_id`),
	FOREIGN KEY (`session_id`) REFERENCES `trajectory_sessions`(`session_id`) ON UPDATE no action ON DELETE cascade
);
--> statement-breakpoint
CREATE UNIQUE INDEX `trajectory_records_by_sequence` ON `trajectory_records` (`session_id`,`sequence`);
--> statement-breakpoint
CREATE INDEX `trajectory_records_by_request` ON `trajectory_records` (`session_id`,`request_id`);
