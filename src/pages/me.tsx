import {useForm} from 'react-hook-form'
import { trpc } from '../utils/trpc';

// Create persona using react-form-hook
// generate a persona  
//
// react-hook-form
// use tailwindcss
const CreatePersonaForm = () => {
  const { register, handleSubmit, watch, formState: { errors } } = useForm();
  const { mutate } = trpc.createPersona.useMutation()
  const onSubmit = data => mutate({name: data.name}) 

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


export default function MePage() {
  const {data} = trpc.getPersonas.useQuery()
  return <div>
    <h2>Manage Your Persona</h2>
    <CreatePersonaForm />  
    {data?.personas?.map(v => (<li key={v.id}>{v.name}</li>))}
  </div>
}
