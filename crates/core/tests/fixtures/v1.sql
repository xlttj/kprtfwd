-- A database as released kprtfwd v1 (v1.3.0) creates it: the schema is copied
-- from pkg/config/sqlite_store.go, user_version stays 0.
CREATE TABLE IF NOT EXISTS port_forwards (
	id TEXT PRIMARY KEY,
	context TEXT NOT NULL,
	namespace TEXT NOT NULL,
	service TEXT NOT NULL,
	port_remote INTEGER NOT NULL,
	port_local INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	name TEXT UNIQUE NOT NULL
);

CREATE TABLE IF NOT EXISTS project_port_forwards (
	project_id INTEGER,
	port_forward_id TEXT,
	FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
	FOREIGN KEY (port_forward_id) REFERENCES port_forwards(id) ON DELETE CASCADE,
	PRIMARY KEY (project_id, port_forward_id)
);

CREATE INDEX IF NOT EXISTS idx_port_forwards_context ON port_forwards(context);
CREATE INDEX IF NOT EXISTS idx_port_forwards_namespace ON port_forwards(namespace);
CREATE INDEX IF NOT EXISTS idx_port_forwards_service ON port_forwards(service);

-- Both ID shapes found in real v1 databases: three and four parts.
INSERT INTO port_forwards (id, context, namespace, service, port_remote, port_local) VALUES
	('staging.mysql.shop', 'staging', 'shop', 'mysql', 3306, 13306),
	('staging.shop.frontend.frontend-80', 'staging', 'shop', 'frontend', 80, 18090),
	('production.shop.frontend.frontend-80', 'production', 'shop', 'frontend', 80, 28090),
	('orbstack.cache.redis.redis-6379', 'orbstack', 'cache', 'redis', 6379, 16379);

INSERT INTO projects (name) VALUES ('shop'), ('empty');

INSERT INTO project_port_forwards (project_id, port_forward_id) VALUES
	(1, 'staging.mysql.shop'),
	(1, 'staging.shop.frontend.frontend-80');
