INSERT INTO users (id, email) VALUES (1, 'first;last@example.invalid');
CREATE UNIQUE INDEX users_email ON users (email);
