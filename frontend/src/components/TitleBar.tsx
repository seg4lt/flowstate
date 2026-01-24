import { Minus, Settings, Square, X } from "lucide-react";
import { Badge } from "./ui/badge";
import { Button } from "./ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "./ui/tooltip";

function sendCommand(cmd: string) {
  // @ts-expect-error - wry IPC
  if (window.ipc?.postMessage) {
    // @ts-expect-error - wry IPC
    window.ipc.postMessage(JSON.stringify({ cmd }));
  }
}

function TrafficLights() {
  return (
    <div className="flex gap-2">
      <button
        className="w-3 h-3 rounded-full bg-[#ff5f57] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => sendCommand("close")}
        aria-label="Close"
      >
        <X className="w-2 h-2 opacity-0 group-hover:opacity-100 text-black/60" />
      </button>
      <button
        className="w-3 h-3 rounded-full bg-[#febc2e] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => sendCommand("minimize")}
        aria-label="Minimize"
      >
        <Minus className="w-2 h-2 opacity-0 group-hover:opacity-100 text-black/60" />
      </button>
      <button
        className="w-3 h-3 rounded-full bg-[#28c840] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => sendCommand("maximize")}
        aria-label="Maximize"
      >
        <Square className="w-1.5 h-1.5 opacity-0 group-hover:opacity-100 text-black/60" />
      </button>
    </div>
  );
}

export function TitleBar() {
  return (
    <div className="h-[38px] bg-secondary border-b border-border flex items-center px-4 titlebar shrink-0">
      <div className="flex items-center gap-3 titlebar-content w-full">
        <TrafficLights />
        <div className="flex items-center gap-2 ml-2">
          <span className="font-semibold text-sm">ZenUI</span>
          <Badge variant="secondary" className="text-[10px] h-4 px-1.5">
            Alpha
          </Badge>
        </div>
        <div className="flex-1" />
        <div className="flex items-center gap-1">
          <Tooltip>
            <TooltipTrigger>
              <Button variant="ghost" size="icon" className="h-7 w-7">
                <Settings className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Settings</TooltipContent>
          </Tooltip>
        </div>
      </div>
    </div>
  );
}
