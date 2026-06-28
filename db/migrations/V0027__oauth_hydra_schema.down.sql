DROP EXTENSION IF EXISTS "uuid-ossp";
ALTER ROLE oauth_hydra RESET search_path;
DROP SCHEMA IF EXISTS oauth_hydra CASCADE;
DROP ROLE IF EXISTS oauth_hydra;
