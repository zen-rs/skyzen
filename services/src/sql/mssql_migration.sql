CREATE TABLE [dbo].[events] (
    id BIGINT NOT NULL PRIMARY KEY,
    [odd;name] NVARCHAR(64) NOT NULL
);
-- seeding; and a semicolon in the comment
INSERT INTO [dbo].[events] (id, [odd;name]) VALUES (1, N'semi;colon');
/* one more ; in a block comment */
SELECT [odd;name] FROM [dbo].[events];
