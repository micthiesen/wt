import { theme } from "../theme.ts";

type Props = {
  title: string;
  items: string[];
  selectedIndex: number;
};

export function PickerModal({ title, items, selectedIndex }: Props) {
  return (
    <box
      position="absolute"
      top="20%"
      left="20%"
      right="20%"
      bottom="20%"
      zIndex={10}
      backgroundColor={theme.bg}
      border
      borderStyle="double"
      borderColor={theme.accent}
      title={` ${title} `}
      titleAlignment="left"
      padding={1}
      flexDirection="column"
    >
      <box flexDirection="column" flexGrow={1}>
        {items.map((item, i) => {
          const selected = i === selectedIndex;
          const bg = selected ? theme.rowSelectedBg : undefined;
          const fg = selected ? theme.fgBright : theme.fg;
          return (
            <box
              key={item}
              flexDirection="row"
              backgroundColor={bg}
              paddingLeft={1}
              paddingRight={1}
            >
              <text fg={selected ? theme.accent : theme.fgDim}>
                {selected ? "▸ " : "  "}
              </text>
              <text fg={fg} wrapMode="none" truncate>
                {item}
              </text>
            </box>
          );
        })}
      </box>
      <box flexShrink={0} paddingLeft={1}>
        <text fg={theme.fgDim}>
          j/k move · ⏎ pick · esc cancel
        </text>
      </box>
    </box>
  );
}
