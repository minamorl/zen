import {
  useEffect,
  useContext,
  createContext,
  useState,
  SetStateAction,
  Dispatch,
} from "react";

export const PersonaContext = createContext<
  [string, Dispatch<SetStateAction<string>>]
>(["", () => {}]);

export function PersonaProvider({ children }: { children: React.ReactNode }) {
  const [persona, setPersona] = useState("");
  // Load initial state from localStorage
  useEffect(() => {
    const storedPersona = localStorage.getItem("persona");
    if (storedPersona) {
      setPersona(JSON.parse(storedPersona));
    }
  }, []);

  // Update localStorage when state changes
  useEffect(() => {
    localStorage.setItem("persona", JSON.stringify(persona));
    console.log("here");
  }, [persona]);
  return (
    <PersonaContext.Provider value={[persona, setPersona]}>
      {children}
    </PersonaContext.Provider>
  );
}

export function usePersona() {
  return useContext(PersonaContext);
}
