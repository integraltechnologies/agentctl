-- Schema version 6 -> 7: the database itself refuses ownership that was not
-- acquired as `Store` acquires it. Existing ownership is kept exactly, even
-- where no current scope would authorize it: dropping it could grant
-- conflicting mutation authority, and only an explicit release ends it. The
-- definitions must match schema.sql.
CREATE INDEX ownership_by_generation ON ownership (generation_id);

CREATE TRIGGER ownership_acquired BEFORE INSERT ON ownership
WHEN EXISTS (SELECT 1 FROM ownership WHERE path = NEW.path)
    OR NOT EXISTS (SELECT 1 FROM generations g
        JOIN tasks t ON t.id = g.task_id
        JOIN plans p ON p.id = t.plan_id
        JOIN task_scope s ON s.task_id = t.id
        WHERE g.id = NEW.generation_id AND g.state = 'active'
            AND p.state <> 'planning' AND s.path = NEW.path)
BEGIN SELECT RAISE(ABORT, 'ownership is not acquirable'); END;
CREATE TRIGGER ownership_not_transferred BEFORE UPDATE ON ownership
BEGIN SELECT RAISE(ABORT, 'ownership is never transferred'); END;
