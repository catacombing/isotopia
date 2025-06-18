-- Create table to store ISO build requests.
CREATE TABLE requests (
    md5sum TEXT NOT NULL,
    device TEXT NOT NULL
        CHECK (device IN ('pinephone', 'pinephone-pro')),
    packages TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'building', 'writing', 'done')),
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    UNIQUE (md5sum, device)
);

-- Update change timestamp whenever status is changed.
CREATE TRIGGER requests_update_timestamp
    AFTER UPDATE OF status
    ON requests
    BEGIN
        UPDATE requests
            SET updated_at = CURRENT_TIMESTAMP
            WHERE md5sum = NEW.md5sum AND device = NEW.device;
    END;
