INSERT INTO _skyzen_migrations (version, name, checksum, applied_at)
VALUES (?, ?, ?, CONVERT(NVARCHAR(33), SYSUTCDATETIME(), 126))
