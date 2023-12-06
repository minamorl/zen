import React, { useEffect } from "react";
import { useConsole } from "../context/consoleContext";
type ConsoleProps = {
  message: string;
};

export const ConsoleUI: React.FC<ConsoleProps> = () => {
  const [message] = useConsole();

  return (
    <div className="text-sm">
      <div className="h-28 flex flex-row">
        <div className="block">{message}</div>
      </div>
    </div>
  );
};
