-- Create table to store ISO build requests.
CREATE TABLE requests (
    packages TEXT PRIMARY KEY NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'building', 'done')),
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Update change timestamp whenever status is changed.
CREATE TRIGGER requests_update_timestamp
    AFTER UPDATE OF status
    ON requests
    BEGIN
        UPDATE requests
            SET updated_at = CURRENT_TIMESTAMP
            WHERE packages = NEW.packages;
    END;
