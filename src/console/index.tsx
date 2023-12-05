import React, { useEffect } from "react";
import { TypeAnimation } from "react-type-animation";
import { useConsole } from "../context/consoleContext";
type ConsoleProps = {
  messages: string[];
};
export const ConsoleUI: React.FC<ConsoleProps> = () => {
  const [message] = useConsole();

  return (
    <div className="text-sm">
      <div className="h-28 flex flex-row">
        <div className="block w-[20px] h-full">$ {message}</div>
      </div>
    </div>
  );
};
