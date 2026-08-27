-- A line comment holding $1 and a bare ? that must survive untouched.
SELECT 'literal $1 and ?' AS quoted,
       $$dollar quoted $1$$ AS dollar_quoted,
       /* block comment holding $2 */ email
FROM users
WHERE id = :p1
  AND email = :p2;
