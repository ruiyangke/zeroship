import {
  createControlClient,
  type AppId,
  type AppRecord,
  type DeployCommand,
  type DeployCommandId,
  type UserId,
} from "../src/index.js";

const appId: AppId = "app_0000000002e4nenowz3qmamtd";
const userId: UserId = "usr_0000000002e4nenowz3qmamtd";
const commandId: DeployCommandId = "dcm_0000000002e4nenowz3qmamtd";
const client = createControlClient({ baseUrl: "https://control.zeroship.ai" });

client.apps.get(appId);
client.env.listVars(appId);
client.egressRules.list(appId);

const app = { id: appId } as AppRecord;
const command: DeployCommand = { id: commandId, archive: new Blob([]) };
client.apps.deploy(appId, command);
void [app];

// @ts-expect-error A platform user id cannot address an app route.
client.apps.get(userId);
// @ts-expect-error An app id is not a platform user id.
const wrongUser: UserId = appId;
void wrongUser;
// @ts-expect-error A deploy uploads a command, never a raw artifact.
client.apps.deploy(appId, new Uint8Array([1]));
// @ts-expect-error An app id is not a deploy command id.
const wrongCommand: DeployCommandId = appId;
void wrongCommand;
