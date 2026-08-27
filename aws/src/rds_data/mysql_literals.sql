-- A line comment holding a ? that must survive untouched.
SELECT 'literal ? mark' AS quoted,
       `weird ? column` AS quoted_identifier,
       /* block comment holding ? */ email
FROM users
WHERE id = ?
  AND email = ?;
