import { useForm, SubmitHandler } from "react-hook-form";
import { useEffect } from "react";
import Select from "react-select";
import { usePersona } from "../context/personaContext"; // Adjust the import path as needed
import { trpc } from "../utils/trpc";
import { useConsole } from "../context/consoleContext";

type Inputs = {
  name: string;
};
const CreatePersonaForm: React.FC<{ refetch: () => void }> = ({ refetch }) => {
  const {
    register,
    handleSubmit,
    watch,
    formState: { errors },
  } = useForm<Inputs>();
  const { mutate } = trpc.createPersona.useMutation();
  const onSubmit: SubmitHandler<Inputs> = (data) => {
    mutate({ name: data.name });
    // refresh
    setTimeout(() => refetch(), 1000);
  };

  return (
    <form onSubmit={handleSubmit(onSubmit)}>
      <input
        className="border-2 border-gray-300 bg-white h-10 px-5 pr-16 rounded-lg text-sm focus:outline-none"
        {...register("name")}
      />
      {errors.name && <span>This field is required</span>}
      <input
        type="submit"
        className="bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded"
        value="Add"
      />
    </form>
  );
};

const PersonaSelector = (props: {
  options: { value: string; label: string }[];
  persona: string;
  handleChange(selectedOption: any): void;
}) => {
  return (
    <Select
      options={props.options}
      value={props.options?.find((option) => option.value === props.persona)}
      onChange={props.handleChange}
      className="text-sm"
    />
  );
};

export default function MePage() {
  const [persona, setPersona] = usePersona();
  const { data: personas, refetch } = trpc.getPersonas.useQuery();
  const [message, setMessage] = useConsole();
  useEffect(() => {
    setMessage("You can switch your persona here");
  }),
    [];

  const options =
    personas?.personas.map((persona) => ({
      value: persona.id,
      label: persona.name,
    })) ?? [];

  const handleChange = (selectedOption: any) => {
    setPersona(selectedOption.value as any);
  };

  return (
    <div className="space-y-4">
      <h2 className="text-2xl font-bold">Switch Your Persona</h2>
      <CreatePersonaForm refetch={refetch} />
      <PersonaSelector
        options={options}
        handleChange={handleChange}
        persona={persona}
      />
      <div>{persona && <p>Selected Persona ID: {persona}</p>}</div>
    </div>
  );
}
