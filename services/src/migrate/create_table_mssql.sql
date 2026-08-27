IF OBJECT_ID(N'_skyzen_migrations', N'U') IS NULL
CREATE TABLE _skyzen_migrations (
    version BIGINT NOT NULL PRIMARY KEY,
    name NVARCHAR(400) NOT NULL,
    checksum NVARCHAR(64) NOT NULL,
    applied_at NVARCHAR(64) NOT NULL
)
