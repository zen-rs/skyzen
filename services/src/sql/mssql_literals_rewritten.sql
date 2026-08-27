-- who? nobody
SELECT [odd?name],
       N'unicode ? literal' AS note, /* another ? in here */
       "quoted?column"
FROM [dbo].[events]
WHERE id = @P1 AND label = @P2
