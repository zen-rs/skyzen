-- Two statements in one file, and a semicolon inside a string literal, so applying this file
-- proves the runner splits on statement boundaries rather than on the `;` byte.
INSERT INTO users (id, email) VALUES (1, 'ada@example.invalid');
INSERT INTO users (id, email) VALUES (2, 'semi;colon@example.invalid');
