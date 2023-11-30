import { useEffect, useState } from 'react';
import { trpc } from '../utils/trpc';
import { SubmitHandler, useForm } from 'react-hook-form';
import Link from 'next/link'

type Inputs = {
  raw_text: string
};


export default function IndexPage() {
  const {data, isLoading, mutate} = trpc.createPost.useMutation()
  const [key, setKey] = useState('invalid')
  const {handleSubmit, register} = useForm<Inputs>()
  const {data: posts, refetch, isRefetching} = trpc.getPosts.useQuery()
  const { data: me} = trpc.getPersonas.useQuery()
  const [selectedPost, setSelectedPost] = useState('')
  const {mutate: createThread} = trpc.createThread.useMutation()
  
  const [threadInput, setThreadInput] = useState('')

  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    mutate({
      raw_text: inputs.raw_text,
      persona_id: me?.personas?.[0].id ?? '' ,
    })
    setKey(inputs.raw_text)
    setTimeout(() => refetch(), 1000)
  }


  if (!posts) {
    return <div>Loading...</div>;
  }
  return (
    <div>
      { me?
        <div>
          Your persona id is {me && Array.isArray(me.personas) && me.personas[0].id}
        </div> : <div>
          <Link href={'/signin'}>SignIn</Link>
        </div>
      }
      <form onSubmit={handleSubmit(onSubmit)} className='shadow p-4 m-4 rounded'>
        <textarea
        className="rounded border-l-1 border-black w-full h-full resize-none"
          {...register("raw_text")}
        />
        <input className='bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded-full' type="submit" value="Post" />
      </form>
      <div>
        <ul>
          { posts.map(v => <><li key={v.id} className="shadow p-4 m-4 rounded" onClick={() => setSelectedPost(v.id)}>
            <div>{v.raw_text}</div>
            <div>{v.created_at}</div>
            
          </li>
            { v.threads.map(x => <li key={x.id} className='shadow p-4 m-4 mx-8 rounded'><div>{x.raw_text}</div></li>)} 
            { selectedPost === v.id && <li key='input' className='shadow p-4 m-4 mx-8 rounded'>
              <textarea onChange={e => setThreadInput(e.currentTarget.value)} value={threadInput} className="rounded border-l-1 border-black w-full h-full resize-none"/>
              <button onClick={() => createThread({
      raw_text: threadInput,
      post_id: v.id,
      persona_id: me?.personas?.[0].id ?? '' ,


              })} className="bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded-full">Create Thread</button>
            </li> }
              
            
          </>) }
          { isRefetching && <li key={key}>{key}</li>}
        </ul>
      </div>
    </div>
  );
}
