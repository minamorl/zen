import React, { useEffect } from "react";
import { useConsole } from "../context/consoleContext";
type ConsoleProps = {
  message: string;
};

export const ConsoleUI: React.FC<ConsoleProps> = () => {
  const [message] = useConsole();

  return (
    <div className="text-sm font-mono">
      <div className="flex flex-row">
        <div className="block w-6">⚡</div>
        <div className="block">{message}</div>
        <div className="block animate-ping">_</div>
      </div>
    </div>
  );
};
