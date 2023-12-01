import {useForm, SubmitHandler} from 'react-hook-form'
import { useEffect } from 'react';
import Select from 'react-select';
import { usePersona } from '../context/personaContext'; // Adjust the import path as needed
import { trpc } from '../utils/trpc';


type Inputs = {
  name: string,
};
const CreatePersonaForm: React.FC<{refetch: () => void}> = ({refetch}) => {

  const { register, handleSubmit, watch, formState: { errors } } = useForm<Inputs>();
  const { mutate } = trpc.createPersona.useMutation()
  const onSubmit: SubmitHandler<Inputs> = data => {
    mutate({name: data.name}) 
    // refresh
    setTimeout(() => refetch(), 1000)
  }

  return (
    <form onSubmit={handleSubmit(onSubmit)}>
      <input className=
        "border-2 border-gray-300 bg-white h-10 px-5 pr-16 rounded-lg text-sm focus:outline-none"

      {...register("name")} />
      {errors.name && <span>This field is required</span>}
      <input type="submit" className='bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded' value='Add' />
      For now first persona is automatically selected
    </form>
  );
};

const PersonaSelector = () => {
  const [persona, setPersona] = usePersona();
  const { data: personas } = trpc.getPersonas.useQuery();

  const options = personas?.personas?.map((persona) => ({
    value: persona.id,
    label: persona.name,
  }));

  const handleChange = (selectedOption: any) => {
    setPersona(selectedOption.value as any);
  };

  useEffect(() => {
    if (personas && personas.length > 0 && !persona) {
      // Automatically set the first persona if none is selected
      setPersona(personas[0].id);
    }
  }, [personas, persona, setPersona]);

  return (
    <Select
      options={options}
      value={options?.find(option => option.value === persona)}
      onChange={handleChange}
      className="text-sm"
    />
  );
};


export default function MePage() {
  const [persona] = usePersona();

  return (
    <div className="space-y-4">
      <h2 className="text-2xl font-bold">Switch Your Persona</h2>
      <CreatePersonaForm refetch={() => {}} />
      <PersonaSelector />
      <div>
        {persona && <p>Selected Persona ID: {persona}</p>}
      </div>
    </div>
  );
}
