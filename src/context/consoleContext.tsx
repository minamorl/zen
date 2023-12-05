import {
  useEffect,
  useContext,
  createContext,
  useState,
  SetStateAction,
  Dispatch,
} from "react";

export const ConsoleContext = createContext<
  [string, Dispatch<SetStateAction<string>>]
>(["", () => {}]);

export function ConsoleProvider({ children }: { children: React.ReactNode }) {
  const [message, setMessage] = useState<string>("");
  return (
    <ConsoleContext.Provider value={[message, setMessage]}>
      {children}
    </ConsoleContext.Provider>
  );
}

export function useConsole() {
  return useContext(ConsoleContext);
}
