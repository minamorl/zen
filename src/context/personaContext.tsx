import { useContext, createContext, useState, SetStateAction, Dispatch} from 'react';

export const PersonaContext = createContext<[string, Dispatch<SetStateAction<string>>] | []>([]);

export function PersonaProvider({ children }: { children: React.ReactNode}) {
  const [persona, setPersona] = useState('');
  return (
    <PersonaContext.Provider value={[persona, setPersona]}>
      {children}
    </PersonaContext.Provider>
  );
}

export function usePersona() {
  return useContext(PersonaContext);
}
