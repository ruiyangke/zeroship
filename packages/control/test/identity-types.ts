import {
  createControlClient,
  type AppId,
  type AppRecord,
  type UserId,
  type WorkflowSignalTokenInput,
} from "../src/index.js";

const appId: AppId = "app_0000000002e4nenowz3qmamtd";
const userId: UserId = "usr_0000000002e4nenowz3qmamtd";
const client = createControlClient({ baseUrl: "https://control.zeroship.ai" });

client.apps.get(appId);
client.env.listVars(appId);
client.egressRules.list(appId);

const app = { id: appId } as AppRecord;
const signal: WorkflowSignalTokenInput = { appId, types: ["ready"], ttl: "1h" };
void [app, signal];

// @ts-expect-error A platform user id cannot address an app route.
client.apps.get(userId);
// @ts-expect-error An app id is not a platform user id.
const wrongUser: UserId = appId;
void wrongUser;
