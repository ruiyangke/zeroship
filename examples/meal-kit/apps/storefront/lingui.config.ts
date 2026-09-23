import { fileURLToPath } from "node:url";
import config from "../../packages/shared/lingui.config";
export default { ...config, rootDir: fileURLToPath(new URL("../../packages/shared", import.meta.url)) };
