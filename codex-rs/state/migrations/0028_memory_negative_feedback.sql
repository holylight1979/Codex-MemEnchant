CREATE TABLE memory_negative_feedback (
    thread_id TEXT NOT NULL,
    source_updated_at INTEGER NOT NULL,
    rollout_slug TEXT,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (thread_id, source_updated_at),
    FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE
);

CREATE INDEX idx_memory_negative_feedback_updated_at
    ON memory_negative_feedback(updated_at DESC, thread_id DESC);
