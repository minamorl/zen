import { useEffect, useState } from 'react';
import { trpc } from '../utils/trpc';
import { SubmitHandler, useForm } from 'react-hook-form';
import Link from 'next/link'
import { format, formatDistance, parseISO } from 'date-fns'
import Uppy from '@uppy/core'
import { Dashboard } from '@uppy/react';
import Tus from '@uppy/tus'

import '@uppy/core/dist/style.min.css';
import '@uppy/dashboard/dist/style.min.css';
import { randomUUID } from 'crypto';

type Inputs = {
  raw_text: string
};

const SUPABASE_ANON_KEY = 'eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNwaXd4bXRrbXltcWx0ZGZ4b2dnIiwicm9sZSI6ImFub24iLCJpYXQiOjE3MDEyNjk1NzMsImV4cCI6MjAxNjg0NTU3M30.aYkKWGpKvfoOj84p5T4lED0Iy6VXK8yH3Jo0h2kEQMc'

const uppy = new Uppy()
        .use(Tus, {
          endpoint: `https://spiwxmtkmymqltdfxogg.supabase.co/storage/v1/upload/resumable`,
          headers: {
            authorization: `Bearer ${SUPABASE_ANON_KEY}`,
            apikey: SUPABASE_ANON_KEY,
          },
          uploadDataDuringCreation: true,
          chunkSize: 6 * 1024 * 1024,
          allowedMetaFields: ['bucketName', 'objectName', 'contentType', 'cacheControl'],
        })

 uppy.on('file-added', (file) => {
        const supabaseMetadata = {
          bucketName: 'images',
          objectName: file.name,
          contentType: file.type,
        }

        file.meta = {
          ...file.meta,
          ...supabaseMetadata,
        }

        console.log('file added', file)
      })
export default function IndexPage() {
  const {data, isLoading, mutate} = trpc.createPost.useMutation()
  const [key, setKey] = useState('invalid')
  const {handleSubmit, register} = useForm<Inputs>()
  const {data: posts, refetch, isRefetching} = trpc.getPosts.useQuery()
  const { data: me} = trpc.getPersonas.useQuery()
  const [selectedPost, setSelectedPost] = useState('')
  const {mutate: createThread} = trpc.createThread.useMutation()
  const {mutate: createResource} = trpc.createResource.useMutation()
  
  const [threadInput, setThreadInput] = useState('')

  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    const r = await uppy.upload()
    
    mutate({
      raw_text: inputs.raw_text,
      persona_id: me?.personas?.[0].id ?? '' ,
    })
    
    if(!data) return

    createResource({
      path: r.successful[0].name,
      post_id: data[0].id
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
          <Dashboard uppy={uppy} hideUploadButton  />
        </form>
      <div>
        <ul>      k
          { posts.map(v => <><li key={v.id} className="shadow p-4 m-4 rounded" onClick={() => setSelectedPost(v.id)}>
            <div>{v.raw_text}</div>
            <div>{v.resources && v?.resources?.[0]?.path}</div>
            <div>{formatDistance(parseISO(v.created_at), new Date(), { addSuffix: true})}</div>
            
          </li>
            { v.threads.map(x => <li key={x.id} className='shadow p-4 m-4 mx-8 rounded'>
              <div>{x.raw_text}</div>
              <div>{formatDistance(parseISO(x.created_at), new Date(), { addSuffix: true})}</div>
            </li>)} 
            { selectedPost === v.id && <li key='input' className='shadow p-4 m-4 mx-8 rounded'>
              <textarea onChange={e => setThreadInput(e.currentTarget.value)} value={threadInput} className="rounded border-l-1 border-black w-full h-full resize-none"/>
              <button onClick={() => {
                createThread({
                  raw_text: threadInput,
                  post_id: v.id,
                  persona_id: me?.personas?.[0].id ?? '' ,
                })
                window.setTimeout(() => refetch(), 500)



              }} className="bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded-full">Create Thread</button>
            </li> }
              
            
          </>) }
          { isRefetching && <li key={key}>{key}</li>}
        </ul>
      </div>
    </div>
  );
}
