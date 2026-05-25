import { createRpcClient, type Stream } from "@zeroship/rpc/client";
import { DefaultChatTransport } from "ai";
import type { UIMessage } from "ai";

type AppRpc = {
  chat: Stream<{ messages: UIMessage[] }, never>;
};

const rpcClient = createRpcClient<AppRpc>({ baseUrl: "" });

export const rpc = {
  chat: rpcClient.stream("chat"),
};

export function chatTransport(handle: {
  streamUrl: (input?: { messages: UIMessage[] }) => string | Promise<string>;
}) {
  return new DefaultChatTransport({
    api: handle.streamUrl() as string,
    prepareSendMessagesRequest: ({ messages }) => ({
      body: { json: { messages } },
    }),
  });
}
