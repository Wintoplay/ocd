DROP INDEX IF EXISTS `automation_due_idx`;
--> statement-breakpoint
CREATE INDEX `automation_active_idx` ON `automation` (`next_due_at`) WHERE "automation"."status" = 'active';
