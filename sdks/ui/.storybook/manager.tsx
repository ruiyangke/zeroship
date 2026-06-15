import React from "react";
import { addons, types, useGlobals } from "storybook/internal/manager-api";
import {
  IconButton,
  TooltipLinkList,
  WithTooltip,
} from "storybook/internal/components";

const ADDON_ID = "zeroship/theme-dropdown";
const TOOL_ID = `${ADDON_ID}/tool`;
const DEFAULT_THEME = "Crystal Light";

const themes = [
  { id: "Crystal Light", title: "Crystal Light" },
  { id: "Crystal Dark", title: "Crystal Dark" },
  { id: "Studio Light", title: "Studio Light" },
  { id: "Ghibli Light", title: "Ghibli Light" },
] as const;

function ThemeDropdownTool() {
  const [{ theme }, updateGlobals] = useGlobals();
  const current =
    typeof theme === "string" && themes.some((item) => item.id === theme)
      ? theme
      : DEFAULT_THEME;

  return (
    <WithTooltip
      placement="top"
      trigger="click"
      closeOnOutsideClick
      tooltip={({ onHide }) => (
        <TooltipLinkList
          links={themes.map((item) => ({
            id: item.id,
            title: item.title,
            active: current === item.id,
            onClick: () => {
              updateGlobals({ theme: item.id });
              onHide();
            },
          }))}
        />
      )}
    >
      <IconButton key={TOOL_ID} active title={`Theme: ${current}`}>
        <span style={{ fontSize: "0.75rem", fontWeight: 600 }}>
          Theme: {current}
        </span>
      </IconButton>
    </WithTooltip>
  );
}

addons.register(ADDON_ID, () => {
  addons.add(TOOL_ID, {
    title: "Theme",
    type: types.TOOL,
    match: ({ viewMode, tabId }) =>
      Boolean(viewMode?.match(/^(story|docs)$/)) && !tabId,
    render: ThemeDropdownTool,
  });
});
